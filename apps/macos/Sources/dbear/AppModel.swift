import AppKit
import DBKit
import Foundation
import SwiftUI
import Observation

enum LoadState<Value> {
    case idle
    case loading
    case loaded(Value)
    case failed(String)

    var value: Value? { if case .loaded(let v) = self { v } else { nil } }
    var isLoading: Bool { if case .loading = self { true } else { false } }
}

// MARK: - Tabs

enum TableTabMode: Hashable {
    case data, structure
}

/// Unsaved changes in a table tab, saved together (⌘S). Rows are identified by their grid id
/// (`Row.id`); new rows get negative ids and are shown above the loaded ones.
struct PendingEdits: Equatable {
    /// Row id → column index → new value.
    var updates: [Int: [Int: EditValue]] = [:]
    var deleted: Set<Int> = []
    var inserted: [InsertedRow] = []

    struct InsertedRow: Equatable, Identifiable {
        let id: Int
        /// One per column; `.default` until typed in.
        var values: [EditValue]
    }

    var isEmpty: Bool { updates.isEmpty && deleted.isEmpty && inserted.isEmpty }
    /// Updated rows that aren't also deleted.
    var updatedRowCount: Int { updates.keys.filter { !deleted.contains($0) }.count }

    /// "2 edited · 1 new · 1 deleted".
    var summary: String {
        var parts: [String] = []
        if updatedRowCount > 0 { parts.append("\(updatedRowCount) edited") }
        if !inserted.isEmpty { parts.append("\(inserted.count) new") }
        if !deleted.isEmpty { parts.append("\(deleted.count) deleted") }
        return parts.joined(separator: " · ")
    }

    func value(row id: Int, column: Int) -> EditValue? {
        if id < 0 { return inserted.first { $0.id == id }?.values[safe: column] }
        return updates[id]?[column]
    }
}

extension Array {
    subscript(safe index: Int) -> Element? { indices.contains(index) ? self[index] : nil }
}

struct CellAddress: Equatable {
    var row: Int
    var column: Int
}

/// Rows that can be edited in a grid: a table tab's, or a script's results (where the core says
/// which cells come from a table whose primary key is in the result).
@MainActor protocol EditableRows: AnyObject {
    var connection: ConnectionConfig { get }
    var edits: PendingEdits { get set }
    var selectedRowIDs: Set<Int> { get set }
    var editRequest: CellAddress? { get set }
    var isReviewingEdits: Bool { get set }
    var isSaving: Bool { get set }
    var saveError: String? { get set }
    var nextInsertedID: Int { get set }
    /// Why no row can be edited (`nil`: some cells can).
    var readOnlyReason: String? { get }
    /// The rows as loaded.
    var loadedRows: QueryResult? { get }
    /// What the review sheet saves to: “users”, or “orders” and “users”.
    var editTarget: String { get }
    var canAddRows: Bool { get }
    var canDeleteRows: Bool { get }
    /// Why `column`'s cells can't be edited (`nil`: they can).
    func columnReadOnly(_ column: Int) -> String?
}

@Observable
@MainActor
final class TableTab: Identifiable {
    let id = UUID()
    let connection: ConnectionConfig
    let table: TableInfo
    /// Preview tabs get replaced by the next table you click (shown in italics).
    var isPreview: Bool
    /// Rows loaded so far (pages are appended as you scroll); `totalCount` comes from the first page.
    var data: LoadState<QueryResult> = .idle
    var isLoadingMore = false
    var loadMoreError: String?
    /// The last page came back short: everything is loaded.
    var reachedEnd = false
    /// Where the next page starts (keyset cursor from the core); `nil` once everything is loaded.
    var nextPage: PageCursor?
    /// Bumped when a load starts, so pages from an older load are dropped.
    var generation = 0
    /// Bumped when a load's rows arrive: tells the grid its rows were replaced (not just appended to).
    /// Not `generation`: rows stay on screen while reloading, so that changes before the new rows exist.
    var dataVersion = 0
    /// Reloading (new sort, refresh) while the previous rows stay on screen.
    var isReloading = false

    /// Rows (`data`) or the table's definition (`structure`).
    var mode = TableTabMode.data
    var structure: LoadState<TableStructure> = .idle
    /// Foreign key links of the rows, made from the structure once per structure and columns.
    @ObservationIgnored private var linkCache: (structure: TableStructure, columns: [ColumnInfo], sources: ResultSources)?

    func linkSources(_ make: (TableStructure, [ColumnInfo]) -> ResultSources) -> ResultSources? {
        guard let structure = structure.value, let columns = data.value?.columns else { return nil }
        if let cache = linkCache, cache.structure == structure, cache.columns == columns { return cache.sources }
        let sources = make(structure, columns)
        linkCache = (structure, columns, sources)
        return sources
    }

    /// The grid's focused cell (row id, column index), shown in the value inspector.
    var focusedCell: CellAddress?

    /// Server-side sort, cycled by clicking column headers.
    var sort: [SortKey] = []
    /// Only the rows matching this `WHERE` condition, e.g. the row a foreign key points at.
    var filter: String?
    /// `filter` for people (`id = 42`), shown in the tab title and the filter bar.
    var filterLabel: String?

    /// Unsaved cell edits, new and deleted rows.
    var edits = PendingEdits()
    /// Grid selection (row ids), for the delete button. Only tracked for editable tabs.
    var selectedRowIDs: Set<Int> = []
    /// Set to start editing a cell (e.g. the first cell of a new row); the grid clears it.
    var editRequest: CellAddress?
    /// Review sheet before saving.
    var isReviewingEdits = false
    /// A direct save (⌘S, no review) is running.
    var isSaving = false
    /// Why the last direct save failed; the review sheet opens to show it.
    var saveError: String?
    var nextInsertedID = -1

    var query: RowQuery { RowQuery(sort: sort, filter: filter) }

    /// Known once the structure has loaded (fetched in the background when the rows load).
    var foreignKeys: [ForeignKeyInfo] { structure.value?.foreignKeys ?? [] }
    /// Other tables' foreign keys pointing at this table (also from the structure).
    var referencedBy: [ReferencingKey] { structure.value?.referencedBy ?? [] }

    /// Why the rows can't be edited, or `nil` if they can.
    var readOnlyReason: String? {
        if table.kind == .view { return "Views are read-only." }
        guard let columns = data.value?.columns else { return "Loading…" }
        if !columns.contains(where: \.isPrimaryKey) {
            return "“\(table.name)” has no primary key, so its rows can’t be identified for editing."
        }
        return nil
    }
    var canLoadMore: Bool { data.value != nil && !reachedEnd && !isLoadingMore && loadMoreError == nil && !isReloading }
    init(connection: ConnectionConfig, table: TableInfo, isPreview: Bool, filter: String? = nil, filterLabel: String? = nil) {
        self.connection = connection
        self.table = table
        self.isPreview = isPreview
        self.filter = filter
        self.filterLabel = filterLabel
    }
}

extension TableTab: EditableRows {
    var loadedRows: QueryResult? { data.value }
    var editTarget: String { "“\(table.name)”" }
    var canAddRows: Bool { readOnlyReason == nil }
    var canDeleteRows: Bool { readOnlyReason == nil }
    func columnReadOnly(_ column: Int) -> String? { readOnlyReason }
}

@Observable
@MainActor
final class ScriptTab: Identifiable {
    let id = UUID()
    let connection: ConnectionConfig
    var title: String
    var text: String
    var result: LoadState<QueryResult> = .idle
    var lastDuration: Duration?
    /// Bumped on every run so the grid knows the result was replaced.
    var runCount = 0
    /// The result grid's focused cell (row index, column index), shown in the value inspector.
    var focusedCell: CellAddress?
    /// The last run was stopped by the user.
    var wasCancelled = false
    /// Editor pane height once the user drags the divider; `nil` = half the available height.
    var editorHeight: CGFloat?
    /// New scripts focus the editor the first time they're shown.
    var needsInitialFocus = true
    /// Editor selection (UTF-16 ranges). Not observed: it changes on every caret move.
    @ObservationIgnored var selectedRanges: [NSRange] = []
    /// Something non-blank is selected, so ⌘↩ runs just that. Only flips when it changes.
    var hasSelection = false

    func updateSelection(_ ranges: [NSRange]) {
        selectedRanges = ranges
        let has = selectedSQL != nil
        if has != hasSelection { hasSelection = has }
    }

    /// Selected text (multiple selections joined by newlines), or `nil` if nothing non-blank is selected.
    var selectedSQL: String? {
        let ns = text as NSString
        let parts = selectedRanges
            .filter { $0.length > 0 && NSMaxRange($0) <= ns.length }
            .map { ns.substring(with: $0) }
        let sql = parts.joined(separator: "\n")
        return sql.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : sql
    }

    /// What ⌘↩ executes: the selection if there is one, otherwise the whole script.
    var sqlToRun: String { selectedSQL ?? text }

    /// The SQL of the last run, for opening its results in a tab of their own.
    var lastRunSQL: String?
    /// A results tab: `text` is the SQL it ran, shown without an editor, read-only. Re-run (⌘↩)
    /// runs it again. Holds the name of the script it came from.
    let resultsOf: String?

    var isResults: Bool { resultsOf != nil }

    /// What the result's columns are (which cells can be edited, foreign key links). Loaded after
    /// each run that returns rows; `nil` meanwhile.
    var sources: ResultSources?
    /// Unsaved cell edits and deleted rows (scripts can't add rows).
    var edits = PendingEdits()
    var selectedRowIDs: Set<Int> = []
    var editRequest: CellAddress?
    var isReviewingEdits = false
    var isSaving = false
    var saveError: String?
    var nextInsertedID = -1

    /// Rows to move to a results tab: the last run returned some, and nothing is running.
    var hasRows: Bool {
        if case .loaded(let result) = result { !result.columns.isEmpty } else { false }
    }

    var loadedRows: QueryResult? {
        if case .loaded(let result) = result, !result.columns.isEmpty { result } else { nil }
    }

    init(connection: ConnectionConfig, title: String, text: String, resultsOf: String? = nil) {
        self.connection = connection
        self.title = title
        self.text = text
        self.resultsOf = resultsOf
    }
}

extension ScriptTab: EditableRows {
    var readOnlyReason: String? {
        guard loadedRows != nil else { return "No rows." }
        guard let sources else { return "Finding the tables these rows come from…" }
        return sources.summaryReason
    }
    var editTarget: String {
        let names = sources?.editableTables.map { "“\($0)”" } ?? []
        return names.count <= 1 ? names.first ?? "the results" : names.dropLast().joined(separator: ", ") + " and " + names.last!
    }
    var canAddRows: Bool { false }
    var canDeleteRows: Bool { sources?.canDeleteRows ?? false }
    func columnReadOnly(_ column: Int) -> String? {
        guard let sources else { return readOnlyReason }
        return sources.readOnlyReason(column: column)
    }
}

enum WorkspaceTab: Identifiable {
    case table(TableTab)
    case script(ScriptTab)
    case users(UsersTab)

    nonisolated var id: UUID {
        switch self {
        case .table(let t): t.id
        case .script(let s): s.id
        case .users(let u): u.id
        }
    }

    @MainActor var connection: ConnectionConfig {
        switch self {
        case .table(let t): t.connection
        case .script(let s): s.connection
        case .users(let u): u.connection
        }
    }

    @MainActor var title: String {
        switch self {
        case .table(let t): t.filterLabel.map { "\(t.table.name) \u{B7} \($0)" } ?? t.table.name
        case .script(let s): s.title
        case .users(let u): u.selected?.reference.title ?? "Users & Roles"
        }
    }

    @MainActor var systemImage: String {
        switch self {
        case .table(let t) where t.filter != nil: "line.3.horizontal.decrease"
        case .table(let t): t.table.kind == .view ? "eye" : "tablecells"
        case .script(let s) where s.isResults: "rectangle.split.3x3"
        case .script: "chevron.left.forwardslash.chevron.right"
        case .users(let u): u.selected.map(\.roleSymbol) ?? "person.2"
        }
    }

    /// Rows that may have unsaved edits.
    @MainActor var editableRows: (any EditableRows)? {
        switch self {
        case .table(let t): t
        case .script(let s): s
        case .users: nil
        }
    }

    @MainActor var isPreview: Bool {
        if case .table(let t) = self { t.isPreview } else { false }
    }
}

// MARK: - App model

/// Opens the connection editor sheet; `original == nil` means a new connection.
struct ConnectionEditorRequest: Identifiable {
    let id = UUID()
    let original: ConnectionConfig?
}

extension ConnectionConfig {
    /// Anything besides name/group changed, so an open connection is stale.
    /// Identifies a driver: one server connection per (connection, database).
    /// Uses the effective database, so an empty `database` and the server default share a driver.
    var driverKey: DriverKey { DriverKey(connectionID: id, database: defaultDatabase) }

    func connectsDifferently(than other: ConnectionConfig) -> Bool {
        (kind, host, port, database, user, sslMode) != (other.kind, other.host, other.port, other.database, other.user, other.sslMode)
            || ssh != other.ssh
    }
}

struct DriverKey: Hashable {
    let connectionID: ConnectionConfig.ID
    let database: String
}

@Observable
@MainActor
final class AppModel {
    /// Saved connections, in user order. Passwords are not included (they're in `secrets`).
    var connections: [ConnectionConfig] = []
    /// Set when the connections file couldn't be read (shown in the sidebar).
    var storeError: String?
    /// The open "New / Edit Connection" sheet.
    var editor: ConnectionEditorRequest?
    /// The "Import from DBeaver" sheet is open.
    var showingImport = false
    /// Connection waiting for delete confirmation.
    var pendingDeletion: ConnectionConfig?
    /// Dumps and restores (see `AppModel+Backup.swift`).
    var dumpRequest: DumpRequest?
    var restoreRequest: RestoreRequest?
    let backups = BackupCenter()

    private let store: ConnectionStore?
    private let secrets: any SecretStore
    /// Open tabs and query history (`state.db`, beside the connection store). See `AppModel+Session.swift`.
    @ObservationIgnored private(set) var state: StateStore?
    /// Bumped when the query history changes, so the History menus read it again.
    var historyVersion = 0
    /// Reopening last session's tabs: nothing is saved meanwhile.
    @ObservationIgnored var isRestoringSession = false

    init(store: ConnectionStore? = nil, secrets: any SecretStore = KeychainSecretStore()) {
        self.secrets = secrets
        do {
            // DBEAR_CONNECTIONS_FILE points at another file (handy for testing).
            let override = ProcessInfo.processInfo.environment["DBEAR_CONNECTIONS_FILE"]
            self.store = try store ?? override.map(ConnectionStore.open(path:)) ?? ConnectionStore.openDefault()
            connections = self.store?.connections() ?? []
        } catch {
            self.store = nil
            storeError = error.localizedDescription
        }
        // Losing it only loses history and the open tabs: not worth an error in the window.
        state = self.store.flatMap { try? StateStore.open(besideStoreAt: $0.path) }
        // A restore may have created or dropped tables.
        backups.onRestored = { [weak self] target in Task { await self?.schemaMayHaveChanged(target) } }
        restoreSession()
        // Script text isn't saved as you type: the last of it is saved when the app quits.
        NotificationCenter.default.addObserver(forName: NSApplication.willTerminateNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.saveSession() }
        }
    }
    var selectedConnectionID: ConnectionConfig.ID? {
        // Show the new connection's cached tables, or a spinner (never the previous connection's).
        didSet {
            guard selectedConnectionID != oldValue else { return }
            selectedDatabase = selectedConnectionID.flatMap { rememberedDatabase(of: $0) }
            showCachedSchemas()
            saveSession()
        }
    }
    /// Database shown for the selected connection; `nil` means the connection's own `database`.
    var selectedDatabase: String? {
        didSet {
            if let id = selectedConnectionID, selectedDatabase != rememberedDatabase(of: id) {
                remember(database: selectedDatabase, of: id)
            }
            if selectedDatabase != oldValue {
                showCachedSchemas()
                saveSession()
            }
        }
    }

    /// The database last shown for a connection, saved with it so it reopens there, also after a
    /// restart (nil: its own). Only for connections that offer their server's other databases.
    private func rememberedDatabase(of id: ConnectionConfig.ID) -> String? {
        guard let connection = connections.first(where: { $0.id == id }),
              connection.showAllDatabases, connection.supportsMultipleDatabases else { return nil }
        if let cached = lastDatabase[id] { return cached }
        let stored = store?.lastDatabase(of: id).flatMap { $0 == connection.defaultDatabase ? nil : $0 }
        lastDatabase[id] = .some(stored)
        return stored
    }

    private func remember(database: String?, of id: ConnectionConfig.ID) {
        lastDatabase[id] = .some(database)
        try? store?.setLastDatabase(database, of: id)
    }

    /// `rememberedDatabase` as read from the store (`.some(nil)`: read, nothing remembered).
    @ObservationIgnored private var lastDatabase: [ConnectionConfig.ID: String?] = [:]
    /// Schemas already listed per database, so switching back and forth doesn't reload them.
    /// Dropped by Refresh, (re)connecting, disconnecting, and scripts that change the schema.
    private var schemaCache: [DriverKey: [Schema]] = [:]
    /// Databases on each connection's server, once listed (only for "show all databases" connections).
    var databaseLists: [ConnectionConfig.ID: [String]] = [:]

    var schemas: LoadState<[Schema]> = .idle
    /// Open "New Database" sheet (see `NewDatabaseSheet.swift`).
    var newDatabaseRequest: NewDatabaseRequest?
    /// Connections whose last attempt failed (shows a warning in the sidebar).
    var failedConnections: Set<ConnectionConfig.ID> = []
    /// Connections with an open server connection (green dot in the sidebar).
    var openConnections: Set<ConnectionConfig.ID> = []

    var tabs: [WorkspaceTab] = [] {
        didSet { saveSession() }
    }
    var activeTabID: UUID? {
        didSet { if activeTabID != oldValue { saveSession() } }
    }

    /// What the middle column lists: tables, or users and roles (toolbar switch).
    var browseMode = BrowseMode.tables
    /// Users & roles per connection, shared by the middle column's list and the users tab.
    /// Kept while the tab is closed; dropped with the schemas (disconnect, edits).
    var usersStates: [ConnectionConfig.ID: UsersTab] = [:]
    /// Users & roles sheets and confirmations (from the middle column's list or the users tab).
    var roleEditor: RoleEditorRequest?
    var privilegeEditor: PrivilegeEditorRequest?
    var pendingRoleDrop: PendingRoleDrop?
    var pendingRevoke: PendingRevoke?
    /// A drop or revoke failed (shown in an alert).
    var accessError: String?

    /// Rows per table page (loaded as you scroll).
    let pageSize = 500
    /// Script results keep at most this many rows; the rest are counted, not kept.
    let scriptRowLimit = 10_000

    /// SQL editor text size (⌘+ / ⌘- / ⌘0). Shared by all script tabs and remembered across launches.
    var editorFontSize: CGFloat = AppModel.storedEditorFontSize {
        didSet { UserDefaults.standard.set(Double(editorFontSize), forKey: Self.editorFontSizeKey) }
    }
    /// The connections column of the split view (⌘B): `.all`, or `.doubleColumn` when hidden.
    var sidebarVisibility: NavigationSplitViewVisibility = .all
    /// The value inspector on the right of the data pane (⌘I). Remembered across launches.
    var showsInspector = UserDefaults.standard.bool(forKey: AppModel.showsInspectorKey) {
        didSet { UserDefaults.standard.set(showsInspector, forKey: Self.showsInspectorKey) }
    }
    private static let showsInspectorKey = "showsInspector"

    static let defaultEditorFontSize = NSFont.systemFontSize
    static let editorFontSizes: ClosedRange<CGFloat> = 8...40

    private var drivers: [DriverKey: any DatabaseDriver] = [:]
    /// Scripts made so far, for their names ("Script 1", "Script 2"…).
    var scriptCounter = 0

    /// Built once per database and reused on every keystroke for SQL completion.
    /// `nil` while loading or if it failed (completion is then just unavailable, nothing fatal).
    private var completionCatalogs: [DriverKey: CompletionCatalog] = [:]
    private var completionCatalogTasks: [DriverKey: Task<Void, Never>] = [:]

    private static let editorFontSizeKey = "editorFontSize"
    private static var storedEditorFontSize: CGFloat {
        let stored = UserDefaults.standard.double(forKey: editorFontSizeKey)
        return stored > 0 ? min(max(CGFloat(stored), editorFontSizes.lowerBound), editorFontSizes.upperBound) : defaultEditorFontSize
    }

    /// A script with an editor is shown (results tabs have none to zoom).
    var isScriptActive: Bool {
        if case .script(let s) = activeTab { !s.isResults } else { false }
    }

    func zoomEditor(by step: CGFloat) {
        editorFontSize = min(max(editorFontSize + step, Self.editorFontSizes.lowerBound), Self.editorFontSizes.upperBound)
    }

    func resetEditorZoom() { editorFontSize = Self.defaultEditorFontSize }

    // MARK: Derived

    var groupedConnections: [(group: String, connections: [ConnectionConfig])] {
        var order: [String] = []
        var byGroup: [String: [ConnectionConfig]] = [:]
        for c in connections {
            if byGroup[c.group] == nil { order.append(c.group) }
            byGroup[c.group, default: []].append(c)
        }
        return order.map { ($0, byGroup[$0]!) }
    }

    var selectedConnection: ConnectionConfig? {
        connections.first { $0.id == selectedConnectionID }
    }

    /// The selected connection pointed at the selected database: what the tables column shows.
    var selectedTarget: ConnectionConfig? {
        guard let connection = selectedConnection else { return nil }
        guard let db = selectedDatabase, db != connection.defaultDatabase else { return connection }
        return connection.withDatabase(db)
    }

    /// Selects a connection and one of its databases (`nil` = the one it showed last, else its default).
    func select(_ connectionID: ConnectionConfig.ID?, database: String? = nil) {
        selectedConnectionID = connectionID
        guard let database else { return }
        let connection = connections.first { $0.id == connectionID }
        let isDefault = database == connection?.database || database == connection?.defaultDatabase
        selectedDatabase = isDefault ? nil : database
    }

    /// Databases offered in the tables column's title menu (nil when there's nothing to choose).
    func databases(of connection: ConnectionConfig) -> [String]? {
        guard connection.showAllDatabases, connection.supportsMultipleDatabases,
              let list = databaseLists[connection.id], !list.isEmpty else { return nil }
        return list
    }

    /// "name" for a connection's own database, "name · other_db" for the rest.
    func displayName(of config: ConnectionConfig) -> String {
        let saved = connections.first { $0.id == config.id }
        guard let saved, saved.defaultDatabase != config.defaultDatabase else { return config.name }
        return "\(config.name) · \(config.defaultDatabase)"
    }

    var activeTab: WorkspaceTab? {
        tabs.first { $0.id == activeTabID }
    }

    /// Highlighted row in the tables column: the active tab's table, if it belongs to the shown connection.
    var selectedTableID: TableInfo.ID? {
        guard case .table(let t) = activeTab, t.connection.driverKey == selectedTarget?.driverKey else { return nil }
        return t.table.id
    }

    func table(withID id: TableInfo.ID) -> TableInfo? {
        schemas.value?.lazy.flatMap(\.tables).first { $0.id == id }
    }

    /// The driver for a connection, created on first use with the latest saved settings
    /// and the password from the Keychain (read only now, so browsing never prompts).
    /// `config.database` picks which database on the server (each gets its own driver).
    func driver(for config: ConnectionConfig) -> any DatabaseDriver {
        if let d = drivers[config.driverKey] { return d }
        var current = connections.first { $0.id == config.id }.map { $0.withDatabase(config.database) } ?? config
        if current.password == nil { current.password = secrets.password(for: config.id) }
        if let ssh = current.ssh, ssh.secret == nil, ssh.auth != .agent {
            current.ssh?.secret = secrets.password(for: SshTunnel.secretAccount(for: config.id))
        }
        let d = Drivers.make(for: current)
        drivers[config.driverKey] = d
        return d
    }

    private func drivers(of id: ConnectionConfig.ID) -> [any DatabaseDriver] {
        drivers.filter { $0.key.connectionID == id }.map(\.value)
    }

    // MARK: Saved connections

    func newConnection() {
        editor = ConnectionEditorRequest(original: nil)
    }

    func edit(_ config: ConnectionConfig) {
        editor = ConnectionEditorRequest(original: config)
    }

    func hasSavedPassword(_ id: ConnectionConfig.ID) -> Bool {
        !id.isEmpty && secrets.hasPassword(for: id)
    }

    func savedPassword(_ id: ConnectionConfig.ID) -> String? {
        id.isEmpty ? nil : secrets.password(for: id)
    }

    /// A connection's saved SSH password or key passphrase.
    func hasSavedSSHSecret(_ id: ConnectionConfig.ID) -> Bool {
        !id.isEmpty && secrets.hasPassword(for: SshTunnel.secretAccount(for: id))
    }

    func savedSSHSecret(_ id: ConnectionConfig.ID) -> String? {
        id.isEmpty ? nil : secrets.password(for: SshTunnel.secretAccount(for: id))
    }

    /// Saves a new or edited connection. `password` / `sshSecret`: nil keeps the saved one, "" removes it.
    /// Changing how to connect drops the live connection and closes its tabs.
    @discardableResult
    func save(_ config: ConnectionConfig, password: String?, sshSecret: String? = nil) throws -> ConnectionConfig {
        guard let store else { throw DatabaseError.storage(storeError ?? "no connections file") }
        let previous = connections.first { $0.id == config.id }
        let saved = try store.upsert(config).refreshed
        if let password {
            if password.isEmpty { secrets.deletePassword(for: saved.id) } else { try secrets.setPassword(password, for: saved.id) }
        }
        // Without a tunnel (or with the agent) there's no SSH secret to keep.
        let sshAccount = SshTunnel.secretAccount(for: saved.id)
        if saved.ssh == nil || saved.ssh?.auth == .agent {
            if previous?.ssh != nil { secrets.deletePassword(for: sshAccount) }
        } else if let sshSecret {
            if sshSecret.isEmpty { secrets.deletePassword(for: sshAccount) } else { try secrets.setPassword(sshSecret, for: sshAccount) }
        }
        if let previous, previous.connectsDifferently(than: saved) || password != nil || sshSecret != nil {
            resetConnection(saved.id)
        } else if let previous, previous.showAllDatabases != saved.showAllDatabases {
            databaseLists[saved.id] = nil
            if saved.id == selectedConnectionID {
                if saved.showAllDatabases { Task { await loadDatabases(saved) } } else { select(saved.id, database: saved.database) }
            }
        }
        connections = store.connections()
        failedConnections.remove(saved.id)
        return saved
    }

    /// Saves a copy (with the same password) and selects it.
    func duplicate(_ config: ConnectionConfig) {
        let fresh = ConnectionConfig(
            id: "", name: "\(config.name) copy", group: config.group, kind: config.kind, host: config.host,
            port: config.port, database: config.database, user: config.user, sslMode: config.sslMode,
            showAllDatabases: config.showAllDatabases, ssh: config.ssh)
        do {
            let saved = try save(fresh, password: savedPassword(config.id), sshSecret: savedSSHSecret(config.id))
            selectedConnectionID = saved.id
        } catch {
            storeError = error.localizedDescription
        }
    }

    func delete(_ config: ConnectionConfig) {
        guard let store else { return }
        do {
            try store.remove(id: config.id)
        } catch {
            storeError = error.localizedDescription
            return
        }
        resetConnection(config.id)
        secrets.deletePassword(for: config.id)
        secrets.deletePassword(for: SshTunnel.secretAccount(for: config.id))
        clearHistory(of: config.id)
        connections = store.connections()
        failedConnections.remove(config.id)
        if selectedConnectionID == config.id {
            selectedConnectionID = nil
            schemas = .idle
        }
    }

    /// Tries a config from the editor without saving it. Returns an error message or nil.
    func test(_ config: ConnectionConfig) async -> String? {
        let driver = Drivers.make(for: config)
        defer { Task { await driver.disconnect() } }
        do {
            try await driver.connect()
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    /// Disconnects and forgets the driver (so the next use picks up new settings) and closes its tabs.
    private func resetConnection(_ id: ConnectionConfig.ID) {
        for key in drivers.keys where key.connectionID == id {
            if let driver = drivers.removeValue(forKey: key) { Task { await driver.disconnect() } }
        }
        databaseLists[id] = nil
        lastDatabase[id] = nil
        forgetSchemas(of: id)
        openConnections.remove(id)
        invalidateCompletionCatalogs(of: id)
        tabs.filter { $0.connection.id == id }.forEach { close($0.id) }
        if id == selectedConnectionID { schemas = .loading; Task { await loadSchemas() } }
    }

    #if DEBUG
    /// Adds the core's sample connections (mock data + the dev database on :54329).
    func addSampleConnections() {
        for sample in Drivers.sampleConnections() where !connections.contains(where: { $0.id == sample.id }) {
            do { try save(sample, password: sample.password) } catch { storeError = error.localizedDescription }
        }
    }
    #endif

    // MARK: Connection state

    func connect(_ config: ConnectionConfig) async {
        do {
            try await driver(for: config).connect()
            failedConnections.remove(config.id)
            if config.id == selectedConnectionID, schemas.value == nil { await loadSchemas() }
            await loadDatabases(config)
        } catch {
            failedConnections.insert(config.id)
        }
        await updateConnectionState(config.id)
    }

    /// Closes the server connection (stopping a running script) and puts the UI back the way it is
    /// at launch for that connection: its tabs close and, if selected, nothing is selected.
    func disconnect(_ config: ConnectionConfig) async {
        let open = drivers(of: config.id)
        guard !open.isEmpty else { return }
        for driver in open { await driver.disconnect() }
        tabs.filter { $0.connection.id == config.id }.forEach { close($0.id) }
        databaseLists[config.id] = nil
        forgetSchemas(of: config.id)
        invalidateCompletionCatalogs(of: config.id)
        lastDatabase[config.id] = nil
        if config.id == selectedConnectionID {
            select(nil)
            schemas = .idle
        }
        await updateConnectionState(config.id)
    }

    func updateConnectionState(_ id: ConnectionConfig.ID) async {
        var open = false
        for driver in drivers(of: id) where await driver.isConnected() {
            open = true
            break
        }
        if open { openConnections.insert(id) } else { openConnections.remove(id) }
    }

    /// Notices connections the server closed. Cheap: only asks drivers that exist, no network I/O.
    func monitorConnections() async {
        while !Task.isCancelled {
            for id in Set(drivers.keys.map(\.connectionID)) { await updateConnectionState(id) }
            try? await Task.sleep(for: .seconds(3))
        }
    }

    // MARK: Schemas

    /// Lists the selected database's schemas. Uses the cached list unless `refresh`.
    func loadSchemas(refresh: Bool = false) async {
        guard let connection = selectedConnection, let target = selectedTarget else { schemas = .idle; return }
        if !refresh, let cached = schemaCache[target.driverKey] {
            schemas = .loaded(cached)
            return
        }
        schemas = .loading
        schemaCache[target.driverKey] = nil
        invalidateCompletionCatalog(for: target)
        // No database configured (MySQL; Postgres falls back to `postgres`): open the server's
        // first one, as if picked from the title menu, rather than listing every database at once.
        if target.defaultDatabase.isEmpty, connection.showAllDatabases, connection.supportsMultipleDatabases {
            if databaseLists[connection.id] == nil { await loadDatabases(connection) }
            if let first = databaseLists[connection.id]?.first, connection.id == selectedConnectionID, selectedDatabase == nil {
                select(connection.id, database: first)  // reloads through the tables column's task
                return
            }
        }
        if databaseLists[connection.id] == nil {
            Task { await loadDatabases(connection) }
        }
        // Only the connection's own database decides the sidebar's warning icon.
        let isDefault = target.driverKey == connection.driverKey
        do {
            let result = try await driver(for: target).listSchemas()
            guard target.driverKey == selectedTarget?.driverKey else { return }
            if isDefault { failedConnections.remove(connection.id) }
            schemaCache[target.driverKey] = result
            schemas = .loaded(result)
        } catch {
            guard target.driverKey == selectedTarget?.driverKey else { return }
            if isDefault { failedConnections.insert(connection.id) }
            schemas = .failed(error.localizedDescription)
        }
        await updateConnectionState(connection.id)
    }

    private func showCachedSchemas() {
        schemas = selectedTarget.flatMap { schemaCache[$0.driverKey] }.map { .loaded($0) } ?? .loading
    }

    private func forgetSchemas(of id: ConnectionConfig.ID) {
        schemaCache = schemaCache.filter { $0.key.connectionID != id }
        usersStates[id] = nil
    }

    /// After a script that may have created, dropped or renamed tables: forget that database's
    /// cached schemas and, if it's on screen, reload them in place (no spinner).
    private func schemaMayHaveChanged(_ target: ConnectionConfig) async {
        let key = target.driverKey
        schemaCache[key] = nil
        invalidateCompletionCatalog(for: target)
        guard key == selectedTarget?.driverKey,
              let result = try? await driver(for: target).listSchemas(),
              key == selectedTarget?.driverKey else { return }
        schemaCache[key] = result
        schemas = .loaded(result)
    }

    /// Statements that can add, remove or rename tables, views or schemas.
    private static let ddl = /(?i)\b(create|drop|alter|rename)\b/

    /// Lists the server's databases for the tables column's database menu.
    func loadDatabases(_ connection: ConnectionConfig) async {
        guard connection.showAllDatabases, connection.supportsMultipleDatabases else { return }
        do {
            let list = try await driver(for: connection).listDatabases()
            databaseLists[connection.id] = list
            // The remembered database is gone (dropped or renamed): back to the connection's own.
            if connection.id == selectedConnectionID, let shown = selectedDatabase, !list.contains(shown) {
                selectedDatabase = nil
            }
        } catch {
            // Not fatal: the connection still works with its own database.
        }
    }

    // MARK: SQL completion

    /// The completion catalog for `connection`, if it's already loaded (`nil` while loading,
    /// if it failed, or if nothing requested it yet — see `loadCompletionCatalogIfNeeded`).
    func completionCatalog(for connection: ConnectionConfig) -> CompletionCatalog? {
        completionCatalogs[connection.driverKey]
    }

    /// Starts loading the catalog in the background if it isn't cached yet. Cheap to call
    /// repeatedly (e.g. from a view's `onAppear`): a load already in flight isn't duplicated.
    func loadCompletionCatalogIfNeeded(for connection: ConnectionConfig) {
        let key = connection.driverKey
        guard completionCatalogs[key] == nil, completionCatalogTasks[key] == nil else { return }
        let driver = driver(for: connection)
        completionCatalogTasks[key] = Task { [weak self] in
            guard let self else { return }
            do {
                async let schemasResult = driver.listSchemas()
                async let columnsResult = driver.listColumns()
                let (schemas, columns) = try await (schemasResult, columnsResult)
                guard !Task.isCancelled else { return }
                completionCatalogs[key] = CompletionCatalog(schemas: schemas, columns: columns)
            } catch {
                // Not fatal: the editor just won't offer schema-aware completion.
            }
            completionCatalogTasks[key] = nil
        }
    }

    private func invalidateCompletionCatalog(for connection: ConnectionConfig) {
        let key = connection.driverKey
        completionCatalogTasks.removeValue(forKey: key)?.cancel()
        completionCatalogs.removeValue(forKey: key)
    }

    private func invalidateCompletionCatalogs(of id: ConnectionConfig.ID) {
        for key in completionCatalogTasks.keys where key.connectionID == id {
            completionCatalogTasks.removeValue(forKey: key)?.cancel()
        }
        completionCatalogs = completionCatalogs.filter { $0.key.connectionID != id }
    }

    // MARK: Tabs

    func activate(_ id: UUID) {
        guard let tab = tabs.first(where: { $0.id == id }) else { return }
        activeTabID = id
        if case .users = tab {
            // Roles are server-wide: keep the database being browsed (the privileges follow it).
            if tab.connection.id != selectedConnectionID { select(tab.connection.id) }
        } else if tab.connection.driverKey != selectedTarget?.driverKey {
            select(tab.connection.id, database: tab.connection.database)
        }
        // The middle column follows the tab: its table, or its role.
        switch tab {
        case .table(let t):
            browseMode = .tables
            // Tabs reopened at launch load when first shown.
            if case .idle = t.data { Task { await load(t) } }
        case .users: browseMode = .users
        case .script: break
        }
    }

    /// Opens a table from the selected connection. Reuses an existing tab for the same table (and
    /// `filter`), otherwise replaces the current preview tab (unless `pinned`).
    func openTable(_ table: TableInfo, pinned: Bool, filter: String? = nil, filterLabel: String? = nil) {
        guard let connection = selectedTarget else { return }

        if let existing = tabs.first(where: {
            if case .table(let t) = $0 {
                t.connection.driverKey == connection.driverKey && t.table.id == table.id && t.filter == filter
            } else { false }
        }) {
            if pinned, case .table(let t) = existing { t.isPreview = false }
            activeTabID = existing.id
            return
        }

        let tab = TableTab(connection: connection, table: table, isPreview: !pinned, filter: filter, filterLabel: filterLabel)
        if !pinned, let previewIndex = tabs.firstIndex(where: \.isPreview) {
            tabs[previewIndex] = .table(tab)
        } else {
            insertAfterActive(.table(tab))
        }
        activeTabID = tab.id
        Task { await load(tab) }
    }

    /// ⌘1…⌘8 select by position, ⌘9 always selects the last tab (Safari behavior).
    func selectTab(number: Int) {
        guard !tabs.isEmpty else { return }
        let index = number == 9 ? tabs.count - 1 : number - 1
        guard tabs.indices.contains(index) else { return }
        activate(tabs[index].id)
    }

    /// Cycles through tabs; wraps around at the ends.
    func selectAdjacentTab(offset: Int) {
        guard !tabs.isEmpty else { return }
        let current = tabs.firstIndex { $0.id == activeTabID } ?? 0
        let next = ((current + offset) % tabs.count + tabs.count) % tabs.count
        activate(tabs[next].id)
    }

    func pin(_ id: UUID) {
        if case .table(let t) = tabs.first(where: { $0.id == id }) { t.isPreview = false }
    }

    func newScript() {
        guard let connection = selectedTarget else { return }
        scriptCounter += 1
        let tab = ScriptTab(connection: connection, title: "Script \(scriptCounter)", text: "")
        insertAfterActive(.script(tab))
        activeTabID = tab.id
        loadCompletionCatalogIfNeeded(for: connection)
    }

    /// ⇧⌘↩: runs the script's selection (or all of it) in a new results tab, after the script.
    func runInNewTab(_ script: ScriptTab) {
        let sql = script.sqlToRun
        guard !sql.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let tab = resultsTab(of: script, sql: sql)
        Task { await run(tab) }
    }

    /// Moves the rows a script shows to a results tab, so its next run doesn't replace them.
    func openResultsInNewTab(_ script: ScriptTab) {
        guard script.hasRows, case .loaded(let result) = script.result else { return }
        let tab = resultsTab(of: script, sql: script.lastRunSQL ?? script.sqlToRun)
        tab.result = .loaded(result)
        tab.lastDuration = script.lastDuration
        tab.lastRunSQL = script.lastRunSQL
        tab.runCount = 1
        tab.sources = script.sources
    }

    /// A results tab for `script` ("Script 1 Results", then "… 2"), opened after the active tab.
    private func resultsTab(of script: ScriptTab, sql: String) -> ScriptTab {
        let base = "\(script.title) Results"
        let taken = Set(tabs.map(\.title))
        var title = base
        var number = 2
        while taken.contains(title) {
            title = "\(base) \(number)"
            number += 1
        }
        let tab = ScriptTab(connection: script.connection, title: title, text: sql, resultsOf: script.title)
        tab.needsInitialFocus = false
        insertAfterActive(.script(tab))
        activeTabID = tab.id
        return tab
    }

    func close(_ id: UUID) {
        guard let index = tabs.firstIndex(where: { $0.id == id }) else { return }
        tabs.remove(at: index)
        guard activeTabID == id else { return }
        if tabs.isEmpty {
            activeTabID = nil
        } else {
            activate(tabs[min(index, tabs.count - 1)].id)
        }
    }

    func closeOthers(than id: UUID) {
        requestClose(tabs.map(\.id).filter { $0 != id })
    }

    /// Closes the tabs after `id` in the tab strip.
    func closeTabs(toTheRightOf id: UUID) {
        guard let index = tabs.firstIndex(where: { $0.id == id }) else { return }
        requestClose(tabs[(index + 1)...].map(\.id))
    }

    func closeAllTabs() {
        requestClose(tabs.map(\.id))
    }

    /// Closes several tabs, asking once first when any has unsaved edits.
    func requestClose(_ ids: [UUID]) {
        let closing = tabs.filter { ids.contains($0.id) }
        let unsaved = closing.compactMap(\.editableRows).filter { !$0.edits.isEmpty }
        if !unsaved.isEmpty {
            let alert = NSAlert()
            alert.messageText = unsaved.count == 1
                ? "Discard unsaved changes to \(unsaved[0].editTarget)?"
                : "Discard unsaved changes in \(unsaved.count) tabs?"
            alert.informativeText = "Closing the tabs throws these changes away."
            alert.addButton(withTitle: "Discard Changes")
            alert.addButton(withTitle: "Cancel")
            alert.buttons[0].hasDestructiveAction = true
            guard alert.runModal() == .alertFirstButtonReturn else { return }
        }
        let wasActive = activeTabID.map(ids.contains) ?? false
        let activeIndex = tabs.firstIndex { $0.id == activeTabID } ?? 0
        tabs.removeAll { ids.contains($0.id) }
        guard wasActive else { return }
        if tabs.isEmpty {
            activeTabID = nil
        } else {
            activate(tabs[min(activeIndex, tabs.count - 1)].id)
        }
    }

    /// Moves a tab to `index` (its position after the move), e.g. while dragging it in the tab strip.
    func moveTab(_ id: UUID, to index: Int) {
        guard let from = tabs.firstIndex(where: { $0.id == id }), tabs.indices.contains(index), from != index
        else { return }
        tabs.insert(tabs.remove(at: from), at: index)
    }

    private func insertAfterActive(_ tab: WorkspaceTab) {
        if let active = tabs.firstIndex(where: { $0.id == activeTabID }) {
            tabs.insert(tab, at: active + 1)
        } else {
            tabs.append(tab)
        }
    }

    // MARK: Loading

    func refreshActiveTab() async {
        switch activeTab {
        case .table(let t) where t.mode == .structure: await loadStructure(t)
        case .table(let t):
            guard confirmDiscardingEdits(in: t) else { return }
            await load(t)
        case .script(let s): await run(s)
        case .users(let u): await loadRoles(u)
        case nil: break
        }
    }

    /// (Re)loads the first page of a table tab with its sort and filter. Rows already shown
    /// stay up while the new ones load, so re-sorting doesn't flash a spinner.
    func load(_ tab: TableTab) async {
        // Edits point at loaded rows by position: new rows make them meaningless.
        tab.edits = PendingEdits()
        tab.selectedRowIDs = []
        tab.generation += 1
        let generation = tab.generation
        if tab.data.value == nil { tab.data = .loading }
        tab.isReloading = true
        tab.isLoadingMore = false
        tab.loadMoreError = nil
        tab.reachedEnd = false
        tab.nextPage = nil
        defer { if generation == tab.generation { tab.isReloading = false } }
        // Foreign keys make cells links to the rows they point at; views have none.
        if tab.table.kind == .table, case .idle = tab.structure {
            Task { await loadStructure(tab) }
        }
        do {
            let page = try await driver(for: tab.connection).fetchPage(
                of: tab.table, query: tab.query, limit: pageSize, after: nil, firstRowID: 0)
            guard generation == tab.generation else { return }
            tab.nextPage = page.next
            tab.reachedEnd = page.next == nil
            tab.dataVersion += 1
            tab.data = .loaded(page.result)
        } catch {
            guard generation == tab.generation else { return }
            tab.data = .failed(error.localizedDescription)
        }
        await updateConnectionState(tab.connection.id)
    }

    /// Appends the next page (called when the grid scrolls near the last loaded row).
    func loadMore(_ tab: TableTab) async {
        guard tab.canLoadMore, let loaded = tab.data.value, let after = tab.nextPage else { return }
        let generation = tab.generation
        tab.isLoadingMore = true
        defer { if generation == tab.generation { tab.isLoadingMore = false } }
        do {
            let page = try await driver(for: tab.connection).fetchPage(
                of: tab.table, query: tab.query, limit: pageSize, after: after, firstRowID: loaded.rows.count)
            guard generation == tab.generation, var current = tab.data.value else { return }
            current.rows += page.result.rows
            tab.nextPage = page.next
            tab.reachedEnd = page.next == nil
            tab.data = .loaded(current)
        } catch {
            guard generation == tab.generation else { return }
            tab.loadMoreError = error.localizedDescription
        }
    }

    /// Header click: sort ascending, then descending, then back to the table's natural order.
    func toggleSort(_ tab: TableTab, column: String) {
        guard confirmDiscardingEdits(in: tab) else { return }
        switch tab.sort.first {
        case let key? where key.column == column && !key.descending:
            tab.sort = [SortKey(column: column, descending: true)]
        case let key? where key.column == column:
            tab.sort = []
        default:
            tab.sort = [SortKey(column: column)]
        }
        tab.isPreview = false
        Task { await load(tab) }
    }

    func setMode(_ mode: TableTabMode, of tab: TableTab) {
        tab.mode = mode
        if mode == .structure, tab.structure.value == nil, !tab.structure.isLoading {
            Task { await loadStructure(tab) }
        }
    }

    func loadStructure(_ tab: TableTab) async {
        tab.structure = .loading
        do {
            tab.structure = .loaded(try await driver(for: tab.connection).describeTable(tab.table))
        } catch {
            tab.structure = .failed(error.localizedDescription)
        }
        await updateConnectionState(tab.connection.id)
    }

    /// Opens the table a foreign key points to (same connection and database as `tab`).
    func openReferencedTable(_ foreignKey: ForeignKeyInfo, from tab: TableTab) {
        openRelated(schema: foreignKey.referencedSchema, name: foreignKey.referencedTable, from: tab.connection)
    }

    /// Opens a table that has a foreign key to `tab`'s table, unfiltered.
    func openReferencingTable(_ key: ReferencingKey, from tab: TableTab) {
        openRelated(schema: key.schema, name: key.table, from: tab.connection)
    }

    /// Foreign key links of a table tab's rows (`nil` until its structure has loaded).
    func linkSources(for tab: TableTab) -> ResultSources? {
        tab.linkSources { driver(for: tab.connection).tableSources(tab.table, columns: $1, structure: $0) }
    }

    /// Opens the row a foreign key cell points at: the referenced table, filtered to the rows whose
    /// referenced columns hold `values` (the cell's row's values for the key's columns, in order).
    func openReferencedRow(_ link: ForeignKeyLink, values: [DBValue], from connection: ConnectionConfig) {
        let target = TableInfo(schema: link.schema, name: link.table)
        Task {
            var columns = link.targetColumns
            if columns.isEmpty {
                // SQLite references the parent's primary key implicitly.
                columns = (try? await driver(for: connection).describeTable(target).primaryKey) ?? []
                guard !columns.isEmpty else { return openRelated(schema: target.schema, name: target.name, from: connection) }
            }
            openRelated(schema: target.schema, name: target.name, from: connection, matching: columns, values: values)
        }
    }

    /// Opens the rows of another table that point at a row through `link`. `values` are the row's
    /// values for the columns the key references, in key order.
    func openReferencingRows(_ link: ReferenceLink, values: [DBValue], from connection: ConnectionConfig) {
        openRelated(schema: link.schema, name: link.table, from: connection, matching: link.columns, values: values)
    }

    /// Opens `schema.name` from `connection` (and its database, switching the tables column to it),
    /// filtered to the rows whose `columns` hold `values` when given.
    private func openRelated(
        schema: String, name: String, from connection: ConnectionConfig, matching columns: [String] = [], values: [DBValue] = []
    ) {
        if connection.driverKey != selectedTarget?.driverKey {
            select(connection.id, database: connection.database)
        }
        let fallback = TableInfo(schema: schema, name: name)
        let target = table(withID: fallback.id) ?? fallback
        guard !columns.isEmpty else { return openTable(target, pinned: true) }
        let filter = RowQuery.matching(columns: columns, values: values, kind: connection.kind)
        let label = zip(columns, values).map { "\($0) = \($1.displayString)" }.joined(separator: ", ")
        openTable(target, pinned: true, filter: filter, filterLabel: label)
    }

    /// Shows every row again in a tab opened on a single referenced row.
    func clearFilter(_ tab: TableTab) {
        guard tab.filter != nil, confirmDiscardingEdits(in: tab) else { return }
        tab.filter = nil
        tab.filterLabel = nil
        Task { await load(tab) }
    }

    // MARK: Editing rows

    /// Asks before throwing away unsaved edits (reloading, re-running, closing). `true`: go ahead.
    func confirmDiscardingEdits(in tab: any EditableRows) -> Bool {
        guard !tab.edits.isEmpty else { return true }
        let alert = NSAlert()
        alert.messageText = "Discard unsaved changes to \(tab.editTarget)?"
        alert.informativeText = "\(tab.edits.summary). Reloading the rows throws these away."
        alert.addButton(withTitle: "Discard Changes")
        alert.addButton(withTitle: "Cancel")
        alert.buttons[0].hasDestructiveAction = true
        guard alert.runModal() == .alertFirstButtonReturn else { return false }
        tab.edits = PendingEdits()
        return true
    }

    /// Stages a cell value. Typing a loaded cell's original value back drops the edit.
    func setCell(_ tab: any EditableRows, row id: Int, column: Int, to value: EditValue) {
        guard tab.columnReadOnly(column) == nil, let result = tab.loadedRows, result.columns.indices.contains(column) else { return }
        if id < 0 {
            guard let index = tab.edits.inserted.firstIndex(where: { $0.id == id }) else { return }
            tab.edits.inserted[index].values[column] = value
        } else {
            guard let original = result.rows[safe: id]?.values[safe: column] else { return }
            var row = tab.edits.updates[id] ?? [:]
            if Self.matches(value, original) { row[column] = nil } else { row[column] = value }
            tab.edits.updates[id] = row.isEmpty ? nil : row
        }
        (tab as? TableTab)?.isPreview = false
    }

    /// Typed text equal to what the cell shows (or NULL left blank) isn't a change.
    private static func matches(_ value: EditValue, _ original: DBValue) -> Bool {
        switch (value, original) {
        case (.null, .null): true
        case (.text(let t), let o): !o.isNull && t == o.displayString
        default: false
        }
    }

    /// A new row at the top of the grid, editing its first editable cell.
    func addRow(_ tab: any EditableRows) {
        guard tab.canAddRows, let columns = tab.loadedRows?.columns else { return }
        let id = tab.nextInsertedID
        tab.nextInsertedID -= 1
        tab.edits.inserted.append(.init(id: id, values: Array(repeating: .default, count: columns.count)))
        (tab as? TableTab)?.isPreview = false
        let first = columns.firstIndex { !$0.isBinary && !($0.isPrimaryKey && columns.filter(\.isPrimaryKey).count == 1) }
        tab.editRequest = CellAddress(row: id, column: first ?? 0)
    }

    /// New rows are dropped; loaded rows are marked for deletion.
    func deleteRows(_ tab: any EditableRows, ids: Set<Int>) {
        guard tab.canDeleteRows, !ids.isEmpty else { return }
        tab.edits.inserted.removeAll { ids.contains($0.id) }
        tab.edits.deleted.formUnion(ids.filter { $0 >= 0 })
        (tab as? TableTab)?.isPreview = false
    }

    func revertRows(_ tab: any EditableRows, ids: Set<Int>) {
        for id in ids {
            tab.edits.updates[id] = nil
            tab.edits.deleted.remove(id)
        }
        tab.edits.inserted.removeAll { ids.contains($0.id) }
    }

    /// Opens the review sheet. A cell still being edited is committed first (ending editing keeps
    /// what was typed), so ⌘S right after typing includes that value.
    func reviewEdits(_ tab: any EditableRows) {
        NSApp.keyWindow?.makeFirstResponder(nil)
        guard !tab.edits.isEmpty else { return }
        tab.isReviewingEdits = true
    }

    /// Saves without the review sheet (⌘S, the toolbar's Save). If it fails, the edits stay and
    /// the review sheet opens with the error and the SQL that was tried.
    func saveEditsNow(_ tab: any EditableRows) {
        NSApp.keyWindow?.makeFirstResponder(nil)
        guard !tab.edits.isEmpty, !tab.isSaving else { return }
        tab.isSaving = true
        Task {
            defer { tab.isSaving = false }
            do {
                try await saveEdits(tab)
            } catch {
                tab.saveError = error.localizedDescription
                tab.isReviewingEdits = true
            }
        }
    }

    func discardEdits(_ tab: any EditableRows) {
        tab.edits = PendingEdits()
        tab.isReviewingEdits = false
    }

    /// The pending edits as core changes, keyed by each row's primary key as loaded.
    func changes(in tab: TableTab) -> [RowChange] {
        guard let result = tab.data.value else { return [] }
        let columns = result.columns
        func key(_ id: Int) -> [KeyValue] {
            guard let row = result.rows[safe: id] else { return [] }
            return columns.indices.filter { columns[$0].isPrimaryKey }.map { KeyValue(column: columns[$0].name, value: row.values[$0]) }
        }
        let edits = tab.edits
        var changes: [RowChange] = []
        for id in edits.deleted.sorted() {
            changes.append(.delete(key: key(id)))
        }
        for (id, cells) in edits.updates.sorted(by: { $0.key < $1.key }) where !edits.deleted.contains(id) {
            let set = cells.sorted { $0.key < $1.key }.map { CellEdit(column: columns[$0.key].name, value: $0.value) }
            changes.append(.update(key: key(id), set: set))
        }
        for row in edits.inserted {
            changes.append(.insert(values: zip(columns, row.values).map { CellEdit(column: $0.name, value: $1) }))
        }
        return changes
    }

    /// A script's pending edits, row by row (the core groups them by table).
    private func resultEdits(in tab: ScriptTab) -> [ResultRowEdit] {
        guard let result = tab.loadedRows else { return [] }
        let edits = tab.edits
        var rows: [ResultRowEdit] = []
        for id in edits.deleted.sorted() {
            if let row = result.rows[safe: id] { rows.append(ResultRowEdit(values: row.values, delete: true)) }
        }
        for (id, cells) in edits.updates.sorted(by: { $0.key < $1.key }) where !edits.deleted.contains(id) {
            if let row = result.rows[safe: id] { rows.append(ResultRowEdit(values: row.values, set: cells)) }
        }
        return rows
    }

    func previewEdits(_ tab: any EditableRows) throws -> [EditStatement] {
        switch tab {
        case let tab as TableTab:
            guard let columns = tab.data.value?.columns else { return [] }
            return try driver(for: tab.connection).previewChanges(of: tab.table, columns: columns, changes: changes(in: tab))
        case let tab as ScriptTab:
            guard let sources = tab.sources else { return [] }
            return try driver(for: tab.connection).previewResultEdits(sources, edits: resultEdits(in: tab))
        default:
            return []
        }
    }

    /// Saves every pending edit in one transaction, then shows the saved rows. Throws (leaving the
    /// edits in place) if anything fails: then nothing was saved.
    func saveEdits(_ tab: any EditableRows) async throws {
        guard !tab.edits.isEmpty else { return }
        switch tab {
        case let tab as TableTab:
            guard let columns = tab.data.value?.columns else { return }
            let changes = changes(in: tab)
            _ = try await driver(for: tab.connection).applyChanges(to: tab.table, columns: columns, changes: changes)
            tab.edits = PendingEdits()
            tab.isReviewingEdits = false
            await load(tab)
        case let tab as ScriptTab:
            guard let sources = tab.sources, let result = tab.loadedRows else { return }
            _ = try await driver(for: tab.connection).applyResultEdits(sources, edits: resultEdits(in: tab))
            // Not re-run: the script may do more than select (`update …; select …`). The saved
            // values are shown as typed instead.
            tab.result = .loaded(Self.applying(tab.edits, to: result))
            tab.runCount += 1
            tab.edits = PendingEdits()
            tab.isReviewingEdits = false
        default:
            break
        }
    }

    /// `result` with saved edits written into its rows (deleted rows dropped, ids renumbered).
    private static func applying(_ edits: PendingEdits, to result: QueryResult) -> QueryResult {
        var saved = result
        var rows: [Row] = []
        for row in result.rows where !edits.deleted.contains(row.id) {
            var values = row.values
            for (column, value) in edits.updates[row.id] ?? [:] where values.indices.contains(column) {
                switch value {
                case .null: values[column] = .null
                case .default: break
                case .text(let text): values[column] = DBValue(typed: text, like: values[column])
                }
            }
            rows.append(Row(id: rows.count, values: values))
        }
        saved.rows = rows
        return saved
    }

    /// Closing a tab from the UI asks first if it has unsaved edits.
    func requestClose(_ id: UUID) {
        if let rows = tabs.first(where: { $0.id == id })?.editableRows, !confirmDiscardingEdits(in: rows) { return }
        close(id)
    }

    var activeTableTab: TableTab? {
        if case .table(let t) = activeTab { t } else { nil }
    }

    /// The active tab's editable rows: a table tab's, or a script's results.
    var activeEditableRows: (any EditableRows)? {
        switch activeTab {
        case .table(let t): t
        case .script(let s): s
        default: nil
        }
    }

    func retryLoadMore(_ tab: TableTab) {
        tab.loadMoreError = nil
        Task { await loadMore(tab) }
    }

    func run(_ tab: ScriptTab) async {
        guard !tab.result.isLoading, confirmDiscardingEdits(in: tab) else { return }
        tab.result = .loading
        tab.runCount += 1
        tab.wasCancelled = false
        tab.sources = nil
        let clock = ContinuousClock()
        let start = clock.now
        do {
            let sql = tab.sqlToRun
            tab.lastRunSQL = sql
            let result = try await driver(for: tab.connection).execute(sql, maxRows: scriptRowLimit)
            tab.lastDuration = clock.now - start
            tab.result = .loaded(result)
            let rows = (result.columns.isEmpty ? result.rowsAffected : result.totalCount ?? result.rows.count).map { UInt64(max(0, $0)) }
            recordHistory(of: tab, sql: sql, rows: rows, error: nil)
            if sql.contains(Self.ddl) { await schemaMayHaveChanged(tab.connection) }
            await describeResult(of: tab)
        } catch DatabaseError.cancelled {
            tab.lastDuration = clock.now - start
            tab.result = .idle
            tab.wasCancelled = true
        } catch {
            tab.lastDuration = clock.now - start
            tab.result = .failed(error.localizedDescription)
            recordHistory(of: tab, sql: tab.lastRunSQL ?? "", rows: nil, error: error.localizedDescription)
        }
        await updateConnectionState(tab.connection.id)
    }

    /// Finds which tables a script's rows come from, for editing them and following their foreign
    /// keys. Shown rows stay read-only (and unlinked) if this fails.
    private func describeResult(of tab: ScriptTab) async {
        guard let result = tab.loadedRows, !result.origins.isEmpty else { return }
        let run = tab.runCount
        let sources = try? await driver(for: tab.connection).describeResult(result)
        if tab.runCount == run { tab.sources = sources }
    }

    /// Stops the script running in `tab` (server-side cancel).
    func cancel(_ tab: ScriptTab) async {
        guard tab.result.isLoading else { return }
        await driver(for: tab.connection).cancel()
    }
}
