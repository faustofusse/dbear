import Foundation

public enum DatabaseKind: String, Sendable, Hashable, CaseIterable {
    case postgres
    case mysql
    case sqlite
    /// Turso / libSQL server (remote, auth token instead of user and password).
    case libsql
    case sqlServer

    public var displayName: String {
        switch self {
        case .postgres: "PostgreSQL"
        case .mysql: "MySQL"
        case .sqlite: "SQLite"
        case .libsql: "Turso"
        case .sqlServer: "SQL Server"
        }
    }
}

/// TLS behaviour, named after libpq's `sslmode`.
public enum SslMode: String, Sendable, Hashable, CaseIterable {
    case disable
    case prefer
    case require
    case verifyFull
}

public struct ConnectionConfig: Identifiable, Hashable, Sendable {
    public let id: String
    public var name: String
    public var group: String
    public var kind: DatabaseKind
    public var host: String
    public var port: Int?
    public var database: String
    public var user: String?
    /// Comes from the Keychain (later); passed to the core per connection.
    public var password: String?
    public var sslMode: SslMode
    /// List every database on the server in the sidebar; `database` is the default one.
    public var showAllDatabases: Bool
    /// e.g. "PostgreSQL · localhost:5432/app_dev" (formatted by the Rust core).
    public var summary: String

    public init(
        id: String, name: String, group: String, kind: DatabaseKind,
        host: String, port: Int? = nil, database: String, user: String? = nil,
        password: String? = nil, sslMode: SslMode = .prefer, showAllDatabases: Bool = true, summary: String = ""
    ) {
        self.id = id
        self.name = name
        self.group = group
        self.kind = kind
        self.host = host
        self.port = port
        self.database = database
        self.user = user
        self.password = password
        self.sslMode = sslMode
        self.showAllDatabases = showAllDatabases
        self.summary = summary
    }

    /// The server's other databases can be switched to (the tables column's title menu), each with
    /// its own session (Postgres, MySQL, SQL Server). SQLite files have none.
    public var supportsMultipleDatabases: Bool { [.postgres, .mysql, .sqlServer].contains(kind) }
}

public enum TableKind: String, Sendable, Hashable {
    case table
    case view
}

public struct TableInfo: Identifiable, Hashable, Sendable {
    public var schema: String
    public var name: String
    public var kind: TableKind
    public var estimatedRowCount: Int?

    public var id: String { "\(schema).\(name)" }

    public init(schema: String, name: String, kind: TableKind = .table, estimatedRowCount: Int? = nil) {
        self.schema = schema
        self.name = name
        self.kind = kind
        self.estimatedRowCount = estimatedRowCount
    }
}

public struct Schema: Identifiable, Hashable, Sendable {
    public var name: String
    public var tables: [TableInfo]
    public var id: String { name }

    public init(name: String, tables: [TableInfo]) {
        self.name = name
        self.tables = tables
    }
}

public struct ColumnInfo: Identifiable, Hashable, Sendable {
    public var name: String
    public var typeName: String
    public var isPrimaryKey: Bool
    public var isNullable: Bool
    public var id: String { name }

    public init(name: String, typeName: String, isPrimaryKey: Bool = false, isNullable: Bool = false) {
        self.name = name
        self.typeName = typeName
        self.isPrimaryKey = isPrimaryKey
        self.isNullable = isNullable
    }
}

/// Columns of one table or view, for SQL completion (`DatabaseDriver.listColumns`).
public struct TableColumns: Sendable {
    public var schema: String
    public var table: String
    public var columns: [ColumnInfo]

    public init(schema: String, table: String, columns: [ColumnInfo]) {
        self.schema = schema
        self.table = table
        self.columns = columns
    }
}

/// One `ORDER BY` term when browsing a table.
public struct SortKey: Hashable, Sendable {
    public var column: String
    public var descending: Bool

    public init(column: String, descending: Bool = false) {
        self.column = column
        self.descending = descending
    }
}

/// Sort and `WHERE` filter for browsing a table. The default is the table's natural order, unfiltered.
public struct RowQuery: Hashable, Sendable {
    /// Applied in order; the core adds the primary key (or row id) so pages stay stable.
    public var sort: [SortKey]
    /// A raw SQL condition (`status = 'paid'`); blank means no filter. The core rejects a `;`.
    public var filter: String?

    public init(sort: [SortKey] = [], filter: String? = nil) {
        self.sort = sort
        self.filter = filter
    }
}

public struct ColumnDetail: Identifiable, Hashable, Sendable {
    public var name: String
    public var typeName: String
    public var isNullable: Bool
    /// Default or generation expression, as the database spells it.
    public var defaultValue: String?
    public var isPrimaryKey: Bool
    public var comment: String?
    public var id: String { name }

    public init(
        name: String, typeName: String, isNullable: Bool, defaultValue: String? = nil,
        isPrimaryKey: Bool = false, comment: String? = nil
    ) {
        self.name = name
        self.typeName = typeName
        self.isNullable = isNullable
        self.defaultValue = defaultValue
        self.isPrimaryKey = isPrimaryKey
        self.comment = comment
    }
}

public struct IndexInfo: Identifiable, Hashable, Sendable {
    public var name: String
    public var columns: [String]
    public var isUnique: Bool
    public var isPrimary: Bool
    /// Full `CREATE INDEX` statement, when the database keeps one.
    public var definition: String?
    public var id: String { name }

    public init(name: String, columns: [String], isUnique: Bool, isPrimary: Bool, definition: String? = nil) {
        self.name = name
        self.columns = columns
        self.isUnique = isUnique
        self.isPrimary = isPrimary
        self.definition = definition
    }
}

public struct ForeignKeyInfo: Identifiable, Hashable, Sendable {
    /// Empty for SQLite, which doesn't name foreign keys.
    public var name: String
    public var columns: [String]
    public var referencedSchema: String
    public var referencedTable: String
    /// Empty when SQLite references the parent's primary key implicitly.
    public var referencedColumns: [String]
    public var onUpdate: String
    public var onDelete: String
    public var id: String { "\(name)|\(columns.joined(separator: ","))|\(referencedSchema).\(referencedTable)" }

    public init(
        name: String, columns: [String], referencedSchema: String, referencedTable: String,
        referencedColumns: [String], onUpdate: String, onDelete: String
    ) {
        self.name = name
        self.columns = columns
        self.referencedSchema = referencedSchema
        self.referencedTable = referencedTable
        self.referencedColumns = referencedColumns
        self.onUpdate = onUpdate
        self.onDelete = onDelete
    }
}

/// A foreign key in another table that points at this one: rows of `schema.table` whose `columns`
/// hold this table's `referencedColumns` belong to that row.
public struct ReferencingKey: Identifiable, Hashable, Sendable {
    public var schema: String
    public var table: String
    /// Empty for SQLite, which doesn't name foreign keys.
    public var name: String
    /// The key's columns, in `table`.
    public var columns: [String]
    /// The columns of this table they point at; empty when SQLite references the primary key implicitly.
    public var referencedColumns: [String]
    public var id: String { "\(schema).\(table)|\(name)|\(columns.joined(separator: ","))" }

    public init(schema: String, table: String, name: String, columns: [String], referencedColumns: [String]) {
        self.schema = schema
        self.table = table
        self.name = name
        self.columns = columns
        self.referencedColumns = referencedColumns
    }
}

/// Columns, keys, indexes, foreign keys and DDL of a table or view.
public struct TableStructure: Hashable, Sendable {
    public var columns: [ColumnDetail]
    /// Primary key columns in key order (empty for views and keyless tables).
    public var primaryKey: [String]
    public var indexes: [IndexInfo]
    public var foreignKeys: [ForeignKeyInfo]
    /// Foreign keys of other tables (in the same database) that point at this one.
    public var referencedBy: [ReferencingKey]
    public var ddl: String?

    public init(
        columns: [ColumnDetail], primaryKey: [String] = [], indexes: [IndexInfo] = [],
        foreignKeys: [ForeignKeyInfo] = [], referencedBy: [ReferencingKey] = [], ddl: String? = nil
    ) {
        self.columns = columns
        self.primaryKey = primaryKey
        self.indexes = indexes
        self.foreignKeys = foreignKeys
        self.referencedBy = referencedBy
        self.ddl = ddl
    }
}

public enum DBValue: Hashable, Sendable {
    case null
    case bool(Bool)
    case int(Int64)
    case double(Double)
    /// Exact numeric (NUMERIC/DECIMAL) kept as text to avoid precision loss.
    case decimal(String)
    case text(String)

    public var isNull: Bool { if case .null = self { true } else { false } }

    public var displayString: String {
        switch self {
        case .null: "NULL"
        case .bool(let b): b ? "true" : "false"
        case .int(let i): String(i)
        case .double(let d): String(d)
        case .decimal(let s), .text(let s): s
        }
    }
}

// MARK: - Editing

/// A new cell value.
public enum EditValue: Hashable, Sendable {
    case null
    /// The column's default: `DEFAULT` in an UPDATE, left out of an INSERT.
    case `default`
    /// Text as typed; the database converts it to the column's type.
    case text(String)
}

public struct CellEdit: Hashable, Sendable {
    public var column: String
    public var value: EditValue

    public init(column: String, value: EditValue) {
        self.column = column
        self.value = value
    }
}

/// A primary key column and the row's current value in it.
public struct KeyValue: Hashable, Sendable {
    public var column: String
    public var value: DBValue

    public init(column: String, value: DBValue) {
        self.column = column
        self.value = value
    }
}

public enum RowChange: Hashable, Sendable {
    case update(key: [KeyValue], set: [CellEdit])
    case insert(values: [CellEdit])
    case delete(key: [KeyValue])
}

/// A statement that saving will run, as shown in the review sheet.
public struct EditStatement: Hashable, Sendable {
    public var sql: String
    /// UPDATE/DELETE by primary key: must match exactly one row, or nothing is saved.
    public var expectOneRow: Bool
    public var target: String
}

extension DBValue {
    /// Text typed into a cell, shown like the value it replaced (after saving, until a reload
    /// reads what the database stored).
    public init(typed text: String, like original: DBValue) {
        switch original {
        case .int: self = Int64(text).map(DBValue.int) ?? .text(text)
        case .double: self = Double(text).map(DBValue.double) ?? .text(text)
        case .decimal: self = Double(text) != nil ? .decimal(text) : .text(text)
        case .bool:
            switch text.lowercased() {
            case "true", "t", "1", "yes": self = .bool(true)
            case "false", "f", "0", "no": self = .bool(false)
            default: self = .text(text)
            }
        default: self = .text(text)
        }
    }
}

public struct Row: Identifiable, Hashable, Sendable {
    public let id: Int
    public var values: [DBValue]

    public init(id: Int, values: [DBValue]) {
        self.id = id
        self.values = values
    }
}

public struct QueryResult: Sendable {
    public var columns: [ColumnInfo]
    public var rows: [Row]
    public var totalCount: Int?
    /// Set for statements that return no rows (INSERT/UPDATE/DDL…).
    public var rowsAffected: Int?
    /// A script result was cut at the row limit; `totalCount` is how many rows it really returned.
    public var truncated: Bool
    /// Script results: the table column each column reads (`nil`: an expression). Empty when unknown.
    public var origins: [ColumnOrigin?]

    public init(
        columns: [ColumnInfo], rows: [Row], totalCount: Int? = nil, rowsAffected: Int? = nil, truncated: Bool = false,
        origins: [ColumnOrigin?] = []
    ) {
        self.columns = columns
        self.rows = rows
        self.totalCount = totalCount
        self.rowsAffected = rowsAffected
        self.truncated = truncated
        self.origins = origins
    }
}

/// The table column a script result column reads, unchanged.
public struct ColumnOrigin: Hashable, Sendable {
    public var schema: String
    public var table: String
    public var column: String

    public init(schema: String, table: String, column: String) {
        self.schema = schema
        self.table = table
        self.column = column
    }
}

/// A foreign key whose columns are all in a grid: its cells open the row it points at.
public struct ForeignKeyLink: Hashable, Sendable {
    /// Grid columns holding the key, in key order.
    public var columns: [Int]
    public var schema: String
    public var table: String
    /// Columns of `table` they match; empty means its primary key (SQLite's implicit reference).
    public var targetColumns: [String]
    /// "Open users Row".
    public var label: String

    public init(columns: [Int], schema: String, table: String, targetColumns: [String], label: String) {
        self.columns = columns
        self.schema = schema
        self.table = table
        self.targetColumns = targetColumns
        self.label = label
    }
}

/// Another table's key pointing at a table in a grid: a row opens the rows that reference it.
public struct ReferenceLink: Hashable, Sendable {
    /// Grid columns holding the values the key references.
    public var values: [Int]
    public var schema: String
    public var table: String
    /// The key's columns, in `table`.
    public var columns: [String]
    /// "orders (user_id)".
    public var label: String

    public init(values: [Int], schema: String, table: String, columns: [String], label: String) {
        self.values = values
        self.schema = schema
        self.table = table
        self.columns = columns
        self.label = label
    }
}

/// One script result row's edits: its values as loaded, new values for some cells, or delete it.
public struct ResultRowEdit: Sendable {
    public var values: [DBValue]
    /// Result column → new value.
    public var set: [Int: EditValue]
    public var delete: Bool

    public init(values: [DBValue], set: [Int: EditValue] = [:], delete: Bool = false) {
        self.values = values
        self.set = set
        self.delete = delete
    }
}

// MARK: - Copying

/// Clipboard formats for grid rows (formatted by the core, `RowFormatter`).
public enum CopyFormat: String, CaseIterable, Sendable {
    case tsv, csv, json, markdown, insert

    public var title: String {
        switch self {
        case .tsv: "Tab-Separated"
        case .csv: "CSV"
        case .json: "JSON"
        case .markdown: "Markdown Table"
        case .insert: "SQL INSERT"
        }
    }
}
