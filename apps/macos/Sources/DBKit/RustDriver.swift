import DBCoreFFI
import Foundation

/// `DatabaseDriver` backed by the shared Rust core through UniFFI.
///
/// Only DBKit knows about the generated `DBCoreFFI` types; the app uses DBKit's own models.
final class RustDriver: DatabaseDriver {
    let config: ConnectionConfig
    private let connection: DBCoreFFI.Connection

    init(config: ConnectionConfig) {
        self.config = config
        self.connection = DBCoreFFI.Connection(config: DBCoreFFI.ConnectionConfig(config))
    }

    static func sampleConnections() -> [ConnectionConfig] {
        DBCoreFFI.sampleConnections().map(ConnectionConfig.init)
    }

    static var coreVersion: String { DBCoreFFI.coreVersion() }

    func connect() async throws {
        try await bridged { try await connection.connect() }
    }

    func disconnect() async {
        await connection.disconnect()
    }

    func isConnected() async -> Bool {
        await connection.isConnected()
    }

    func listDatabases() async throws -> [String] {
        try await bridged { try await connection.listDatabases() }
    }

    func listSchemas() async throws -> [Schema] {
        try await bridged { try await connection.listSchemas() }.map(Schema.init)
    }

    func listColumns() async throws -> [TableColumns] {
        try await bridged { try await connection.listColumns() }.map(TableColumns.init)
    }

    func fetchRows(of table: TableInfo, query: RowQuery, limit: Int, offset: Int) async throws -> QueryResult {
        let page = try await bridged {
            try await connection.fetchRows(
                table: DBCoreFFI.TableInfo(table), query: DBCoreFFI.RowQuery(query),
                limit: UInt32(clamping: limit), offset: UInt64(max(0, offset)))
        }
        return QueryResult(page, firstRowID: offset)
    }

    func fetchPage(of table: TableInfo, query: RowQuery, limit: Int, after: PageCursor?, firstRowID: Int) async throws -> TablePage {
        let page = try await bridged {
            try await connection.fetchPage(
                table: DBCoreFFI.TableInfo(table), query: DBCoreFFI.RowQuery(query),
                limit: UInt32(clamping: limit), after: after?.token)
        }
        return TablePage(result: QueryResult(page.result, firstRowID: firstRowID), next: page.nextCursor.map(PageCursor.init))
    }

    func previewChanges(of table: TableInfo, columns: [ColumnInfo], changes: [RowChange]) throws -> [EditStatement] {
        do {
            return try connection.previewChanges(
                table: DBCoreFFI.TableInfo(table), columns: columns.map(DBCoreFFI.ColumnInfo.init),
                changes: changes.map(DBCoreFFI.RowChange.init)
            ).map { EditStatement(sql: $0.sql, expectOneRow: $0.expectOneRow, target: $0.target) }
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }

    func applyChanges(to table: TableInfo, columns: [ColumnInfo], changes: [RowChange]) async throws -> Int {
        let affected = try await bridged {
            try await connection.applyChanges(
                table: DBCoreFFI.TableInfo(table), columns: columns.map(DBCoreFFI.ColumnInfo.init),
                changes: changes.map(DBCoreFFI.RowChange.init))
        }
        return Int(clamping: affected)
    }

    func describeTable(_ table: TableInfo) async throws -> TableStructure {
        TableStructure(try await bridged { try await connection.describeTable(table: DBCoreFFI.TableInfo(table)) })
    }

    func execute(_ sql: String, maxRows: Int?) async throws -> QueryResult {
        let limit = maxRows.map { UInt32(clamping: max(0, $0)) }
        return QueryResult(try await bridged { try await connection.execute(sql: sql, maxRows: limit) }, firstRowID: 0)
    }

    func cancel() async {
        await connection.cancel()
    }

    func listRoles() async throws -> [Role] {
        try await bridged { try await connection.listRoles() }.map(Role.init)
    }

    func listGrants(of role: RoleRef) async throws -> [ObjectPrivileges] {
        let grants = try await bridged { try await connection.listGrants(role: DBCoreFFI.RoleRef(role)) }
        return DBCoreFFI.groupGrants(grants: grants).map {
            ObjectPrivileges(object: GrantObject($0.object), privileges: PrivilegeSet($0.privileges))
        }
    }

    func listDatabaseAccess(of role: RoleRef) async throws -> [DatabaseAccess] {
        try await bridged { try await connection.listDatabaseAccess(role: DBCoreFFI.RoleRef(role)) }.map {
            DatabaseAccess(
                database: $0.database, privileges: PrivilegeSet($0.privileges),
                everyoneCanConnect: $0.everyoneCanConnect, isOwner: $0.isOwner, level: DatabaseLevel($0.level))
        }
    }

    func databaseLevel(of role: RoleRef, in database: String) async throws -> DatabaseLevelContext {
        DatabaseLevelContext(try await bridged { try await connection.databaseLevel(role: DBCoreFFI.RoleRef(role), database: database) })
    }

    func previewAccess(_ changes: [AccessChange]) throws -> [AccessStatement] {
        do {
            return try connection.previewAccess(changes: changes.map(DBCoreFFI.AccessChange.init))
                .map { AccessStatement(sql: $0.sql, display: $0.display) }
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }

    func applyAccess(_ changes: [AccessChange]) async throws {
        try await bridged { try await connection.applyAccess(changes: changes.map(DBCoreFFI.AccessChange.init)) }
    }

    /// Rethrows core errors as `DatabaseError`.
    private func bridged<T>(_ body: () async throws -> T) async throws -> T {
        do {
            return try await body()
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }
}

// MARK: - Conversions (FFI ⇄ DBKit)

extension DatabaseError {
    init(_ error: DBCoreFFI.DbError) {
        switch error {
        case .ConnectionFailed(let message): self = .connectionFailed(message)
        case .TableNotFound(let name): self = .tableNotFound(name)
        case .Unsupported(let message): self = .unsupported(message)
        case .Query(let message): self = .query(message)
        case .Cancelled: self = .cancelled
        case .InvalidConfig(let message): self = .invalidConfig(message)
        case .Storage(let message): self = .storage(message)
        case .Internal(let message): self = .internal(message)
        }
    }
}

extension DatabaseKind {
    init(_ kind: DBCoreFFI.DatabaseKind) {
        switch kind {
        case .postgres: self = .postgres
        case .mysql: self = .mysql
        case .sqlite: self = .sqlite
        case .libsql: self = .libsql
        case .sqlServer: self = .sqlServer
        }
    }
}

extension DBCoreFFI.DatabaseKind {
    init(_ kind: DatabaseKind) {
        switch kind {
        case .postgres: self = .postgres
        case .mysql: self = .mysql
        case .sqlite: self = .sqlite
        case .libsql: self = .libsql
        case .sqlServer: self = .sqlServer
        }
    }
}

extension SslMode {
    init(_ mode: DBCoreFFI.SslMode) {
        switch mode {
        case .disable: self = .disable
        case .prefer: self = .prefer
        case .require: self = .require
        case .verifyFull: self = .verifyFull
        }
    }
}

extension DBCoreFFI.SslMode {
    init(_ mode: SslMode) {
        switch mode {
        case .disable: self = .disable
        case .prefer: self = .prefer
        case .require: self = .require
        case .verifyFull: self = .verifyFull
        }
    }
}

extension ConnectionConfig {
    init(_ c: DBCoreFFI.ConnectionConfig) {
        self.init(
            id: c.id, name: c.name, group: c.group, kind: DatabaseKind(c.kind),
            host: c.host, port: c.port.map(Int.init), database: c.database, user: c.user,
            password: c.password, sslMode: SslMode(c.sslMode), showAllDatabases: c.showAllDatabases,
            summary: DBCoreFFI.connectionSummary(config: c)
        )
    }
}

extension DBCoreFFI.ConnectionConfig {
    init(_ c: ConnectionConfig) {
        self.init(
            id: c.id, name: c.name, group: c.group, kind: DBCoreFFI.DatabaseKind(c.kind),
            host: c.host, port: c.port.map { UInt16(clamping: $0) }, database: c.database, user: c.user,
            password: c.password, sslMode: DBCoreFFI.SslMode(c.sslMode), showAllDatabases: c.showAllDatabases
        )
    }
}

extension TableKind {
    init(_ kind: DBCoreFFI.TableKind) {
        switch kind {
        case .table: self = .table
        case .view: self = .view
        }
    }
}

extension TableInfo {
    init(_ t: DBCoreFFI.TableInfo) {
        self.init(schema: t.schema, name: t.name, kind: TableKind(t.kind),
                  estimatedRowCount: t.estimatedRowCount.map { Int(clamping: $0) })
    }
}

extension DBCoreFFI.TableInfo {
    init(_ t: TableInfo) {
        self.init(schema: t.schema, name: t.name, kind: t.kind == .view ? .view : .table,
                  estimatedRowCount: t.estimatedRowCount.map { UInt64(max(0, $0)) })
    }
}

extension Schema {
    init(_ s: DBCoreFFI.Schema) {
        self.init(name: s.name, tables: s.tables.map(TableInfo.init))
    }
}

extension DBCoreFFI.Schema {
    init(_ s: Schema) {
        self.init(name: s.name, tables: s.tables.map(DBCoreFFI.TableInfo.init))
    }
}

extension TableColumns {
    init(_ t: DBCoreFFI.TableColumns) {
        self.init(schema: t.schema, table: t.table, columns: t.columns.map(ColumnInfo.init))
    }
}

extension DBCoreFFI.TableColumns {
    init(_ t: TableColumns) {
        self.init(schema: t.schema, table: t.table, columns: t.columns.map(DBCoreFFI.ColumnInfo.init))
    }
}

extension ColumnInfo {
    init(_ c: DBCoreFFI.ColumnInfo) {
        self.init(name: c.name, typeName: c.typeName, isPrimaryKey: c.isPrimaryKey, isNullable: c.isNullable)
    }
}

extension DBCoreFFI.ColumnInfo {
    init(_ c: ColumnInfo) {
        self.init(name: c.name, typeName: c.typeName, isPrimaryKey: c.isPrimaryKey, isNullable: c.isNullable)
    }
}

extension ColumnInfo {
    /// Binary columns are shown as a hex preview, so their cells can't be edited (decided by the core).
    public var isBinary: Bool { DBCoreFFI.isBinaryColumn(column: DBCoreFFI.ColumnInfo(self)) }
}

extension RowQuery {
    /// A `WHERE` filter for the rows whose `columns` hold `values`, spelled for `kind` (by the core),
    /// e.g. `"id" = 42` for the row a foreign key points at.
    public static func matching(columns: [String], values: [DBValue], kind: DatabaseKind) -> String {
        DBCoreFFI.matchFilter(kind: DBCoreFFI.DatabaseKind(kind), columns: columns, values: values.map(DBCoreFFI.Value.init))
    }
}

extension DBCoreFFI.Value {
    init(_ v: DBValue) {
        switch v {
        case .null: self = .null
        case .bool(let b): self = .bool(b)
        case .int(let i): self = .int(i)
        case .double(let d): self = .float(d)
        case .decimal(let s): self = .decimal(s)
        case .text(let s): self = .text(s)
        }
    }
}

extension DBCoreFFI.RowChange {
    init(_ change: RowChange) {
        func keys(_ key: [KeyValue]) -> [DBCoreFFI.KeyValue] {
            key.map { DBCoreFFI.KeyValue(column: $0.column, value: DBCoreFFI.Value($0.value)) }
        }
        func edits(_ set: [CellEdit]) -> [DBCoreFFI.CellEdit] {
            set.map { edit in
                let value: DBCoreFFI.EditValue = switch edit.value {
                case .null: .null
                case .default: .default
                case .text(let text): .text(text: text)
                }
                return DBCoreFFI.CellEdit(column: edit.column, value: value)
            }
        }
        switch change {
        case .update(let key, let set): self = .update(key: keys(key), set: edits(set))
        case .insert(let values): self = .insert(values: edits(values))
        case .delete(let key): self = .delete(key: keys(key))
        }
    }
}

extension DBCoreFFI.RowQuery {
    init(_ q: RowQuery) {
        self.init(sort: q.sort.map { DBCoreFFI.SortKey(column: $0.column, descending: $0.descending) }, filter: q.filter)
    }
}

extension TableStructure {
    init(_ s: DBCoreFFI.TableStructure) {
        self.init(
            columns: s.columns.map {
                ColumnDetail(name: $0.name, typeName: $0.typeName, isNullable: $0.isNullable,
                             defaultValue: $0.defaultValue, isPrimaryKey: $0.isPrimaryKey, comment: $0.comment)
            },
            primaryKey: s.primaryKey,
            indexes: s.indexes.map {
                IndexInfo(name: $0.name, columns: $0.columns, isUnique: $0.isUnique, isPrimary: $0.isPrimary,
                          definition: $0.definition)
            },
            foreignKeys: s.foreignKeys.map {
                ForeignKeyInfo(name: $0.name, columns: $0.columns, referencedSchema: $0.referencedSchema,
                               referencedTable: $0.referencedTable, referencedColumns: $0.referencedColumns,
                               onUpdate: $0.onUpdate, onDelete: $0.onDelete)
            },
            referencedBy: s.referencedBy.map {
                ReferencingKey(schema: $0.schema, table: $0.table, name: $0.name, columns: $0.columns,
                               referencedColumns: $0.referencedColumns)
            },
            ddl: s.ddl
        )
    }
}

extension DBValue {
    init(_ v: DBCoreFFI.Value) {
        switch v {
        case .null: self = .null
        case .bool(let b): self = .bool(b)
        case .int(let i): self = .int(i)
        case .float(let d): self = .double(d)
        case .decimal(let s): self = .decimal(s)
        case .text(let s): self = .text(s)
        }
    }
}

// MARK: - Completion

/// Schemas, tables/views and columns for one connection, built once and reused on every
/// keystroke. The only other place besides `RustDriver` that touches `DBCoreFFI` directly
/// (see `AGENTS.md`): the wrapped type stays private to this file.
public final class CompletionCatalog: @unchecked Sendable {
    private let inner: DBCoreFFI.CompletionCatalog

    public init(schemas: [Schema], columns: [TableColumns]) {
        inner = DBCoreFFI.CompletionCatalog(schemas: schemas.map(DBCoreFFI.Schema.init), columns: columns.map(DBCoreFFI.TableColumns.init))
    }

    /// Completions for `text` with the caret at UTF-16 offset `location`.
    public func complete(text: String, location: Int, kind: DatabaseKind) -> Completions {
        let result = inner.complete(text: text, location: UInt32(clamping: max(0, location)), kind: DBCoreFFI.DatabaseKind(kind))
        return Completions(
            range: NSRange(location: Int(result.location), length: Int(result.length)),
            items: result.items.map(CompletionItem.init)
        )
    }

    /// Completions inside `table`'s `WHERE` filter: `text` is just the condition, and `location`
    /// and the returned range are UTF-16 offsets into it.
    public func completeFilter(text: String, location: Int, kind: DatabaseKind, table: TableInfo) -> Completions {
        let result = inner.completeFilter(
            text: text, location: UInt32(clamping: max(0, location)), kind: DBCoreFFI.DatabaseKind(kind),
            schema: table.schema, table: table.name)
        return Completions(
            range: NSRange(location: Int(result.location), length: Int(result.length)),
            items: result.items.map(CompletionItem.init)
        )
    }
}

extension CompletionKind {
    init(_ kind: DBCoreFFI.CompletionKind) {
        switch kind {
        case .keyword: self = .keyword
        case .schema: self = .schema
        case .table: self = .table
        case .view: self = .view
        case .column: self = .column
        case .function: self = .function
        }
    }
}

extension CompletionItem {
    init(_ i: DBCoreFFI.CompletionItem) {
        self.init(label: i.label, insertText: i.insertText, kind: CompletionKind(i.kind), detail: i.detail)
    }
}

extension QueryResult {
    init(_ r: DBCoreFFI.QueryResult, firstRowID: Int) {
        self.init(
            columns: r.columns.map(ColumnInfo.init),
            rows: r.rows.enumerated().map { Row(id: firstRowID + $0.offset, values: $0.element.map(DBValue.init)) },
            totalCount: r.totalCount.map { Int(clamping: $0) },
            rowsAffected: r.rowsAffected.map { Int(clamping: $0) },
            truncated: r.truncated
        )
    }
}

// MARK: - Dump & restore

extension RustDriver {
    static func dump(
        _ config: ConnectionConfig, to url: URL, options: DumpOptions, cancellation: BackupCancellation,
        progress: @escaping @Sendable (DumpProgress) -> Void
    ) async throws -> DumpSummary {
        let handle = DBCoreFFI.CancelHandle()
        cancellation.onCancel { handle.cancel() }
        do {
            let summary = try await DBCoreFFI.dumpDatabase(
                config: DBCoreFFI.ConnectionConfig(config), path: url.path, options: DBCoreFFI.DumpOptions(options),
                listener: DumpListenerBox(progress), cancel: handle)
            return DumpSummary(tables: Int(summary.tables), rows: Int(clamping: summary.rows),
                               bytes: Int(clamping: summary.bytes), warnings: summary.warnings)
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }

    static func restore(
        _ config: ConnectionConfig, from url: URL, options: RestoreOptions, cancellation: BackupCancellation,
        progress: @escaping @Sendable (RestoreProgress) -> Void
    ) async throws -> RestoreSummary {
        let handle = DBCoreFFI.CancelHandle()
        cancellation.onCancel { handle.cancel() }
        do {
            let s = try await DBCoreFFI.restoreDatabase(
                config: DBCoreFFI.ConnectionConfig(config), path: url.path,
                options: DBCoreFFI.RestoreOptions(singleTransaction: options.singleTransaction, stopOnError: options.stopOnError),
                listener: RestoreListenerBox(progress), cancel: handle)
            return RestoreSummary(statements: Int(clamping: s.statements), rows: Int(clamping: s.rows), errors: s.errors,
                                  errorCount: Int(s.errorCount), warnings: s.warnings)
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }

    static func defaultDumpFileName(database: String, date: String, compression: DumpCompression) -> String {
        DBCoreFFI.defaultDumpFileName(database: database, date: date, compression: compression == .gzip ? .gzip : .none)
    }
}

private final class DumpListenerBox: DBCoreFFI.DumpListener, @unchecked Sendable {
    let handler: @Sendable (DumpProgress) -> Void
    init(_ handler: @escaping @Sendable (DumpProgress) -> Void) { self.handler = handler }

    func onProgress(progress p: DBCoreFFI.DumpProgress) {
        let phase: DumpPhase = switch p.phase {
        case .connecting: .connecting
        case .schema: .schema
        case .data: .data
        case .postData: .postData
        case .finishing: .finishing
        }
        handler(DumpProgress(
            phase: phase, object: p.object, tablesDone: Int(p.tablesDone), tablesTotal: Int(p.tablesTotal),
            rowsDone: Int(clamping: p.rowsDone), tableRowsDone: Int(clamping: p.tableRowsDone),
            tableRowsEstimate: p.tableRowsEstimate.map { Int(clamping: $0) }, bytesWritten: Int(clamping: p.bytesWritten)))
    }
}

private final class RestoreListenerBox: DBCoreFFI.RestoreListener, @unchecked Sendable {
    let handler: @Sendable (RestoreProgress) -> Void
    init(_ handler: @escaping @Sendable (RestoreProgress) -> Void) { self.handler = handler }

    func onProgress(progress p: DBCoreFFI.RestoreProgress) {
        handler(RestoreProgress(bytesRead: Int(clamping: p.bytesRead), bytesTotal: Int(clamping: p.bytesTotal),
                                statements: Int(clamping: p.statements), errors: Int(p.errors)))
    }
}

extension DBCoreFFI.DumpOptions {
    init(_ o: DumpOptions) {
        let content: DBCoreFFI.DumpContent = switch o.content {
        case .schemaAndData: .schemaAndData
        case .schemaOnly: .schemaOnly
        case .dataOnly: .dataOnly
        }
        let scope: DBCoreFFI.DumpScope = switch o.scope {
        case .database: .database
        case .schemas(let names): .schemas(schemas: names)
        case .tables(let tables): .tables(tables: tables.map(DBCoreFFI.TableInfo.init))
        }
        self.init(
            content: content, scope: scope, compression: o.compression == .gzip ? .gzip : .none,
            dataStyle: o.dataStyle == .insert ? .insert : .copy, dropObjects: o.dropObjects, createDatabase: o.createDatabase)
    }
}

// MARK: - Copying rows

public enum RowFormatter {
    /// Rows as clipboard text in `format`. `schema`/`table` name the `INSERT` target
    /// (`nil` for script results); `headers` adds a header line to TSV/CSV.
    public static func format(
        _ rows: [[DBValue]], columns: [ColumnInfo], as format: CopyFormat, kind: DatabaseKind,
        schema: String? = nil, table: String? = nil, headers: Bool = false
    ) -> String {
        let ffiFormat: DBCoreFFI.CopyFormat = switch format {
        case .tsv: .tsv
        case .csv: .csv
        case .json: .json
        case .markdown: .markdown
        case .insert: .insert
        }
        return DBCoreFFI.formatRows(
            format: ffiFormat, kind: DBCoreFFI.DatabaseKind(kind), schema: schema, table: table,
            columns: columns.map(DBCoreFFI.ColumnInfo.init), rows: rows.map { $0.map(DBCoreFFI.Value.init) },
            headers: headers)
    }

    /// A JSON object or array re-indented for reading (key order and digits kept), else `nil`.
    public static func prettyJSON(_ text: String) -> String? {
        DBCoreFFI.prettyJson(text: text)
    }
}

// MARK: - Users & privileges

extension Access {
    /// What can be managed on `kind`; `nil` if users can't be managed there.
    public static func features(_ kind: DatabaseKind) -> AccessFeatures? {
        DBCoreFFI.accessFeatures(kind: DBCoreFFI.DatabaseKind(kind)).map {
            AccessFeatures(
                hosts: $0.hosts, superuser: $0.superuser, createDB: $0.createDb, createRole: $0.createRole,
                validUntil: $0.validUntil, connectionLimit: $0.connectionLimit, membership: $0.membership,
                grantsPerDatabase: $0.grantsPerDatabase, objectKinds: $0.objectKinds.map(GrantObjectKind.init))
        }
    }

    /// Database access levels to offer on `kind`, from least to most.
    public static func levels(_ kind: DatabaseKind) -> [DatabaseLevel] {
        DBCoreFFI.databaseLevels(kind: DBCoreFFI.DatabaseKind(kind)).map(DatabaseLevel.init)
    }

    /// A random password (letters, digits and URL-safe symbols).
    public static func generatePassword(length: Int = 24) throws -> String {
        do {
            return try DBCoreFFI.generatePassword(length: UInt32(clamping: length))
        } catch let error as DBCoreFFI.DbError {
            throw DatabaseError(error)
        }
    }

    /// The privileges that exist on `object` in `kind`, in display order.
    public static func privileges(_ kind: DatabaseKind, on object: GrantObjectKind) -> [String] {
        DBCoreFFI.accessPrivileges(kind: DBCoreFFI.DatabaseKind(kind), object: DBCoreFFI.GrantObjectKind(object))
    }
}

extension RoleRef {
    init(_ r: DBCoreFFI.RoleRef) { self.init(name: r.name, host: r.host) }
}

extension DBCoreFFI.RoleRef {
    init(_ r: RoleRef) { self.init(name: r.name, host: r.host) }
}

extension Role {
    init(_ r: DBCoreFFI.Role) {
        self.init(
            name: r.name, host: r.host, canLogin: r.canLogin, isSuperuser: r.isSuperuser, canCreateDB: r.canCreateDb,
            canCreateRole: r.canCreateRole, isSystem: r.isSystem, connectionLimit: r.connectionLimit.map(Int.init),
            validUntil: r.validUntil, memberOf: r.memberOf.map(RoleRef.init), comment: r.comment)
    }
}

extension DBCoreFFI.Role {
    init(_ r: Role) {
        self.init(
            name: r.name, host: r.host, canLogin: r.canLogin, isSuperuser: r.isSuperuser, canCreateDb: r.canCreateDB,
            canCreateRole: r.canCreateRole, isSystem: r.isSystem, connectionLimit: r.connectionLimit.map { UInt32(clamping: $0) },
            validUntil: r.validUntil, memberOf: r.memberOf.map(DBCoreFFI.RoleRef.init), comment: r.comment)
    }
}

extension DBCoreFFI.RoleSpec {
    init(_ s: RoleSpec) {
        self.init(
            name: s.name, host: s.host, password: s.password.isEmpty ? nil : s.password, canLogin: s.canLogin,
            isSuperuser: s.isSuperuser, canCreateDb: s.canCreateDB, canCreateRole: s.canCreateRole,
            connectionLimit: s.connectionLimit.map { UInt32(clamping: $0) }, validUntil: s.validUntil,
            memberOf: s.memberOf.map(DBCoreFFI.RoleRef.init))
    }
}

extension GrantObjectKind {
    init(_ k: DBCoreFFI.GrantObjectKind) {
        self = switch k {
        case .server: .server
        case .database: .database
        case .schema: .schema
        case .table: .table
        case .sequence: .sequence
        case .allTables: .allTables
        case .allSequences: .allSequences
        }
    }
}

extension DBCoreFFI.GrantObjectKind {
    init(_ k: GrantObjectKind) {
        self = switch k {
        case .server: .server
        case .database: .database
        case .schema: .schema
        case .table: .table
        case .sequence: .sequence
        case .allTables: .allTables
        case .allSequences: .allSequences
        }
    }
}

extension GrantObject {
    init(_ o: DBCoreFFI.GrantObject) {
        self = switch o {
        case .server: .server
        case .database(let name): .database(name)
        case .schema(let name): .schema(name)
        case .table(let schema, let name): .table(schema: schema, name: name)
        case .sequence(let schema, let name): .sequence(schema: schema, name: name)
        case .allTables(let schema): .allTables(schema: schema)
        case .allSequences(let schema): .allSequences(schema: schema)
        }
    }
}

extension DBCoreFFI.GrantObject {
    init(_ o: GrantObject) {
        self = switch o {
        case .server: .server
        case .database(let name): .database(name: name)
        case .schema(let name): .schema(name: name)
        case .table(let schema, let name): .table(schema: schema, name: name)
        case .sequence(let schema, let name): .sequence(schema: schema, name: name)
        case .allTables(let schema): .allTables(schema: schema)
        case .allSequences(let schema): .allSequences(schema: schema)
        }
    }
}

extension PrivilegeSet {
    init(_ s: DBCoreFFI.PrivilegeSet) { self.init(privileges: s.privileges, grantable: s.grantable) }
}

extension DBCoreFFI.PrivilegeSet {
    init(_ s: PrivilegeSet) { self.init(privileges: s.privileges, grantable: s.grantable) }
}

extension DBCoreFFI.AccessChange {
    init(_ c: AccessChange) {
        self = switch c {
        case .createRole(let spec): .createRole(spec: DBCoreFFI.RoleSpec(spec))
        case .alterRole(let role, let spec): .alterRole(role: DBCoreFFI.Role(role), spec: DBCoreFFI.RoleSpec(spec))
        case .dropRole(let role): .dropRole(role: DBCoreFFI.RoleRef(role))
        case .setPrivileges(let role, let object, let before, let after):
            .setPrivileges(
                role: DBCoreFFI.RoleRef(role), object: DBCoreFFI.GrantObject(object),
                before: DBCoreFFI.PrivilegeSet(before), after: DBCoreFFI.PrivilegeSet(after))
        case .setDatabaseLevel(let role, let context, let level):
            .setDatabaseLevel(role: DBCoreFFI.RoleRef(role), context: DBCoreFFI.DatabaseLevelContext(context), level: DBCoreFFI.DatabaseLevel(level))
        }
    }
}

extension DatabaseLevel {
    init(_ l: DBCoreFFI.DatabaseLevel) {
        self = switch l {
        case .noAccess: .noAccess
        case .connect: .connect
        case .readOnly: .readOnly
        case .readWrite: .readWrite
        case .schemaChanges: .schemaChanges
        case .custom: .custom
        }
    }

    /// "Read only"…
    public var title: String { DBCoreFFI.databaseLevelTitle(level: DBCoreFFI.DatabaseLevel(self)) }

    /// What the level allows on `kind`, in a sentence.
    public func summary(_ kind: DatabaseKind) -> String {
        DBCoreFFI.databaseLevelSummary(level: DBCoreFFI.DatabaseLevel(self), kind: DBCoreFFI.DatabaseKind(kind))
    }
}

extension DBCoreFFI.DatabaseLevel {
    init(_ l: DatabaseLevel) {
        self = switch l {
        case .noAccess: .noAccess
        case .connect: .connect
        case .readOnly: .readOnly
        case .readWrite: .readWrite
        case .schemaChanges: .schemaChanges
        case .custom: .custom
        }
    }
}

extension DatabaseLevelContext {
    init(_ c: DBCoreFFI.DatabaseLevelContext) {
        self.init(database: c.database, level: DatabaseLevel(c.level), privileges: PrivilegeSet(c.privileges), schemas: c.schemas, owners: c.owners)
    }
}

extension DBCoreFFI.DatabaseLevelContext {
    init(_ c: DatabaseLevelContext) {
        self.init(database: c.database, level: DBCoreFFI.DatabaseLevel(c.level), privileges: DBCoreFFI.PrivilegeSet(c.privileges), schemas: c.schemas, owners: c.owners)
    }
}
