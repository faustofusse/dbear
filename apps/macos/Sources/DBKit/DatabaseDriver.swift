import Foundation

public enum DatabaseError: Error, Sendable, LocalizedError {
    case connectionFailed(String)
    case tableNotFound(String)
    case unsupported(String)
    case query(String)
    case cancelled
    case invalidConfig(String)
    case storage(String)
    case `internal`(String)

    public var errorDescription: String? {
        switch self {
        case .connectionFailed(let msg): "Connection failed: \(msg)"
        case .tableNotFound(let name): "Table not found: \(name)"
        case .unsupported(let what): "Unsupported: \(what)"
        case .query(let msg): msg
        case .cancelled: "Query cancelled"
        case .invalidConfig(let msg): msg
        case .storage(let msg): "Couldn’t save connections: \(msg)"
        case .internal(let msg): "Internal error: \(msg)"
        }
    }
}

/// The boundary the SwiftUI app talks to. Backed by the Rust core (`RustDriver`);
/// other conformances (e.g. previews) can stand in without touching the UI.
public protocol DatabaseDriver: Sendable {
    var config: ConnectionConfig { get }
    func connect() async throws
    func disconnect() async
    /// Whether a server connection is open right now (false after the server drops it).
    func isConnected() async -> Bool
    /// Databases on the same server this login can open (just the configured one for SQLite).
    func listDatabases() async throws -> [String]
    func listSchemas() async throws -> [Schema]
    /// Columns of every table and view this connection can see, for SQL completion.
    func listColumns() async throws -> [TableColumns]
    /// One page of a table, sorted and filtered by `query`. `totalCount` is only set for the
    /// first page (`offset == 0`), and may be nil for a filtered big table.
    func fetchRows(of table: TableInfo, query: RowQuery, limit: Int, offset: Int) async throws -> QueryResult
    /// The page after `after` (`nil`: the first page), in the same order as `fetchRows`.
    /// Row ids start at `firstRowID`. `totalCount` is only set for the first page.
    func fetchPage(of table: TableInfo, query: RowQuery, limit: Int, after: PageCursor?, firstRowID: Int) async throws -> TablePage
    /// Columns, keys, indexes, foreign keys and DDL of a table or view.
    func describeTable(_ table: TableInfo) async throws -> TableStructure
    /// The SQL `applyChanges` would run, in order. `columns` are the table's loaded columns.
    func previewChanges(of table: TableInfo, columns: [ColumnInfo], changes: [RowChange]) throws -> [EditStatement]
    /// Saves row edits in one transaction, all or nothing. Returns the rows affected.
    func applyChanges(to table: TableInfo, columns: [ColumnInfo], changes: [RowChange]) async throws -> Int
    /// Runs a script keeping at most `maxRows` rows (nil = all); see `QueryResult.truncated`.
    func execute(_ sql: String, maxRows: Int?) async throws -> QueryResult
    /// Stops the running `execute`, which then throws `DatabaseError.cancelled`.
    func cancel() async
    /// Users and roles on the server, system ones included (see `Access.features`).
    func listRoles() async throws -> [Role]
    /// Privileges granted directly to `role`, grouped by object (Postgres: in this database).
    func listGrants(of role: RoleRef) async throws -> [ObjectPrivileges]
    /// The SQL `applyAccess` would run (passwords masked in `display`).
    func previewAccess(_ change: AccessChange) throws -> [AccessStatement]
    /// Creates, changes or drops a role, or changes its privileges.
    func applyAccess(_ change: AccessChange) async throws
}

extension DatabaseDriver {
    /// One page of a table in its natural order, unfiltered.
    public func fetchRows(of table: TableInfo, limit: Int, offset: Int) async throws -> QueryResult {
        try await fetchRows(of: table, query: RowQuery(), limit: limit, offset: offset)
    }

    /// Runs a script and keeps every row.
    public func execute(_ sql: String) async throws -> QueryResult {
        try await execute(sql, maxRows: nil)
    }
}

public enum Drivers {
    public static func make(for config: ConnectionConfig) -> any DatabaseDriver {
        RustDriver(config: config)
    }

    /// Sample connections (mock data + the dev database), for development.
    public static func sampleConnections() -> [ConnectionConfig] {
        RustDriver.sampleConnections()
    }

    public static var coreVersion: String { RustDriver.coreVersion }
}
