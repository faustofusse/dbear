import DBCoreFFI
import Foundation

/// Saved connections, persisted by the Rust core in SQLite (no passwords).
/// Passwords go through a `SecretStore` (the Keychain in the app).
public final class ConnectionStore: @unchecked Sendable {
    private let inner: DBCoreFFI.ConnectionStore

    private init(_ inner: DBCoreFFI.ConnectionStore) {
        self.inner = inner
    }

    /// `~/Library/Application Support/dbear/dbear.db` (imports an old `connections.json` once).
    public static func openDefault() throws -> ConnectionStore {
        try bridged { ConnectionStore(try DBCoreFFI.ConnectionStore.openDefault()) }
    }

    public static func open(path: String) throws -> ConnectionStore {
        try bridged { ConnectionStore(try DBCoreFFI.ConnectionStore.open(path: path)) }
    }

    public var path: String { inner.path() }

    /// In user order; `password` is always nil.
    public func connections() -> [ConnectionConfig] {
        inner.connections().map(ConnectionConfig.init)
    }

    /// Adds or replaces (by id) and saves. An empty id gets a new one.
    /// Returns the stored config (validated, trimmed, password stripped).
    @discardableResult
    public func upsert(_ config: ConnectionConfig) throws -> ConnectionConfig {
        try bridged { ConnectionConfig(try inner.upsert(config: DBCoreFFI.ConnectionConfig(config))) }
    }

    @discardableResult
    public func remove(id: ConnectionConfig.ID) throws -> Bool {
        try bridged { try inner.remove(id: id) }
    }

    /// The database last browsed on a connection (forgotten when its `database` is edited).
    public func lastDatabase(of id: ConnectionConfig.ID) -> String? {
        inner.lastDatabase(id: id)
    }

    /// Remembers the database browsed on a connection; nil = its own `database`.
    public func setLastDatabase(_ database: String?, of id: ConnectionConfig.ID) throws {
        try bridged { try inner.setLastDatabase(id: id, database: database) }
    }
}

extension ConnectionConfig {
    /// Blank config for the "New Connection" form (empty id = not saved yet).
    public static func blank(_ kind: DatabaseKind = .postgres) -> ConnectionConfig {
        ConnectionConfig(DBCoreFFI.newConnectionConfig(kind: DBCoreFFI.DatabaseKind(kind)))
    }

    /// Parses `postgres://user:pass@host:5432/db?sslmode=require` (also mysql://, sqlite://, libsql://…?authToken=).
    public static func parse(url: String) throws -> ConnectionConfig {
        try bridged { ConnectionConfig(try DBCoreFFI.parseConnectionUrl(url: url)) }
    }

    /// First problem preventing a save, or nil.
    public var validationError: String? {
        DBCoreFFI.validateConnection(config: DBCoreFFI.ConnectionConfig(self))
    }

    public func url(includingPassword: Bool = false) -> String {
        DBCoreFFI.connectionUrl(config: DBCoreFFI.ConnectionConfig(self), includePassword: includingPassword)
    }

    /// The database actually opened: `database`, or the server default when it's empty
    /// (Postgres: `postgres`; empty for MySQL).
    public var defaultDatabase: String {
        DBCoreFFI.defaultDatabase(config: DBCoreFFI.ConnectionConfig(self))
    }

    /// Name used when `name` is left empty: the database, else the host.
    public var defaultName: String {
        DBCoreFFI.defaultConnectionName(config: DBCoreFFI.ConnectionConfig(self))
    }

    /// The same connection (same id and login) pointed at another database on the server.
    public func withDatabase(_ database: String) -> ConnectionConfig {
        var copy = self
        copy.database = database
        return copy.refreshed
    }

    /// The same config with `summary` recomputed (after editing fields).
    public var refreshed: ConnectionConfig {
        ConnectionConfig(DBCoreFFI.ConnectionConfig(self))
    }
}

extension DatabaseKind {
    public var defaultPort: Int? {
        DBCoreFFI.defaultPort(kind: DBCoreFFI.DatabaseKind(self)).map(Int.init)
    }
}

/// Rethrows core errors as `DatabaseError`.
func bridged<T>(_ body: () throws -> T) throws -> T {
    do {
        return try body()
    } catch let error as DBCoreFFI.DbError {
        throw DatabaseError(error)
    }
}
