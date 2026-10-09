import DBCoreFFI
import Foundation

/// One script run, from a connection's query history.
public struct QueryHistoryEntry: Identifiable, Hashable, Sendable {
    public let id: Int64
    public let connectionID: String
    /// The database it ran in (empty: the connection's own).
    public let database: String
    public let sql: String
    public let ranAt: Date
    public let duration: Duration?
    /// Rows returned, or affected for statements that return none.
    public let rows: UInt64?
    /// Why it failed (`nil`: it succeeded).
    public let error: String?
}

/// What the app remembers between launches besides connections: open tabs (JSON under a key) and
/// each connection's query history. Kept by the Rust core in `state.db`, beside the connection
/// store, so the GPUI app on Linux reads and writes the same file.
public final class StateStore: @unchecked Sendable {
    private let inner: DBCoreFFI.StateStore

    private init(_ inner: DBCoreFFI.StateStore) {
        self.inner = inner
    }

    /// `state.db` in the folder of the connection store at `storePath`.
    public static func open(besideStoreAt storePath: String) throws -> StateStore {
        try bridged { StateStore(try DBCoreFFI.StateStore.openBeside(storePath: storePath)) }
    }

    public static func open(path: String) throws -> StateStore {
        try bridged { StateStore(try DBCoreFFI.StateStore.open(path: path)) }
    }

    public var path: String { inner.path() }

    public func value(forKey key: String) -> String? {
        inner.get(key: key)
    }

    /// Saves `value` under `key`; nil removes it.
    public func setValue(_ value: String?, forKey key: String) throws {
        try bridged { try inner.set(key: key, value: value) }
    }

    /// Records a run. Running the same SQL again in the same database moves it to the top.
    public func addHistory(
        connectionID: String, database: String, sql: String, duration: Duration?, rows: UInt64?, error: String?
    ) throws {
        let ms = duration.map { UInt64(max(0, $0.components.seconds * 1000 + $0.components.attoseconds / 1_000_000_000_000_000)) }
        try bridged {
            try inner.addHistory(entry: NewHistoryEntry(
                connectionId: connectionID, database: database, sql: sql, durationMs: ms, rows: rows, error: error))
        }
    }

    /// A connection's newest runs first (any database).
    public func history(of connectionID: String, limit: Int = 25) throws -> [QueryHistoryEntry] {
        try bridged {
            try inner.history(connectionId: connectionID, limit: UInt32(max(0, limit))).map {
                QueryHistoryEntry(
                    id: $0.id, connectionID: $0.connectionId, database: $0.database, sql: $0.sql,
                    ranAt: Date(timeIntervalSince1970: Double($0.ranAt) / 1000),
                    duration: $0.durationMs.map { .milliseconds(Int64($0)) }, rows: $0.rows, error: $0.error)
            }
        }
    }

    public func clearHistory(of connectionID: String) throws {
        try bridged { try inner.clearHistory(connectionId: connectionID) }
    }
}
