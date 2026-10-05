import DBKit
import SwiftUI

/// Grant or revoke a role's privileges on one object: pick the object, check what it should have.
/// Applying grants what was added and revokes what was removed, showing the SQL first.
struct PrivilegeEditorSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let tab: UsersTab
    let role: RoleRef
    /// Editing an existing grant: the object is fixed.
    let initialObject: GrantObject?

    @State private var kind: GrantObjectKind
    @State private var database = ""
    @State private var schema = ""
    @State private var table = ""
    @State private var after = PrivilegeSet()
    /// Schemas (MySQL: databases) and their tables, for the pickers.
    @State private var schemas: [Schema] = []
    @State private var databases: [String] = []
    @State private var loading = true
    @State private var saving = false
    @State private var error: String?

    init(tab: UsersTab, role: RoleRef, initialObject: GrantObject?) {
        self.tab = tab
        self.role = role
        self.initialObject = initialObject
        let fallback = tab.features.objectKinds.first { $0 == .table } ?? tab.features.objectKinds.first ?? .table
        _kind = State(initialValue: initialObject?.kind ?? fallback)
        switch initialObject {
        case .database(let name): _database = State(initialValue: name)
        case .schema(let name), .allTables(let name), .allSequences(let name): _schema = State(initialValue: name)
        case .table(let s, let name), .sequence(let s, let name):
            _schema = State(initialValue: s)
            _table = State(initialValue: name)
        case .server, nil: break
        }
    }

    private var dbKind: DatabaseKind { tab.connection.kind }
    private var isFixed: Bool { initialObject != nil }

    /// Kinds offered for a new grant (a single sequence can only be edited, not picked).
    private var kinds: [GrantObjectKind] {
        tab.features.objectKinds.filter { $0 != .sequence || kind == .sequence }
    }

    /// The object picked so far, or `nil` while incomplete.
    private var object: GrantObject? {
        if let initialObject { return initialObject }
        switch kind {
        case .server: return .server
        case .database:
            let name = dbKind == .postgres ? tab.connection.defaultDatabase : database
            return name.isEmpty ? nil : .database(name)
        case .schema: return schema.isEmpty ? nil : .schema(schema)
        case .allTables: return schema.isEmpty ? nil : .allTables(schema: schema)
        case .allSequences: return schema.isEmpty ? nil : .allSequences(schema: schema)
        case .table: return schema.isEmpty || table.isEmpty ? nil : .table(schema: schema, name: table)
        case .sequence: return nil
        }
    }

    /// What the role holds on `object` now. For "all tables" that's what every table there has.
    private var before: PrivilegeSet {
        guard let object else { return PrivilegeSet() }
        if case .allTables(let schema) = object {
            let tables = schemas.first { $0.name == schema }?.tables ?? []
            let sets = tables.map { tab.privileges(on: .table(schema: schema, name: $0.name)) }
            guard let first = sets.first else { return PrivilegeSet() }
            let common = sets.dropFirst().reduce(Set(first.privileges)) { $0.intersection($1.privileges) }
            return PrivilegeSet(
                privileges: first.privileges.filter(common.contains),
                grantable: !common.isEmpty && sets.allSatisfy(\.grantable))
        }
        return tab.privileges(on: object)
    }

    /// Checkboxes: the object's privileges, plus any odd ones it already holds.
    private var available: [String] {
        let known = Access.privileges(dbKind, on: kind)
        return known + before.privileges.filter { !known.contains($0) }
    }

    private var change: AccessChange? {
        object.map { .setPrivileges(role: role, object: $0, before: before, after: after) }
    }

    private var preview: Result<[AccessStatement], Error>? {
        guard let change else { return nil }
        return Result { try model.previewAccess(change, in: tab) }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Form {
                objectSection
                privilegesSection
                sqlSection
            }
            .formStyle(.grouped)
            .scrollBounceBehavior(.basedOnSize)
            .disabled(loading)
            footer
        }
        .frame(width: 560, height: 620)
        .task { await loadObjects() }
        .onChange(of: object) { after = before }
        .onChange(of: after) { error = nil }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Image(systemName: "key.fill")
                .font(.system(size: 17))
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(.tint, in: Circle())
            VStack(alignment: .leading, spacing: 2) {
                Text(isFixed ? "Privileges on \(initialObject?.title ?? "")" : "Grant Privileges").font(.headline)
                Text("For \(role.title)" + (tab.features.grantsPerDatabase ? " in \(tab.connection.defaultDatabase)" : ""))
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if loading { ProgressView().controlSize(.small) }
        }
        .padding(.horizontal, 20)
        .padding(.top, 20)
    }

    // MARK: Object

    @ViewBuilder
    private var objectSection: some View {
        Section {
            Picker("Type", selection: $kind) {
                ForEach(kinds, id: \.self) { Text($0.title).tag($0) }
            }
            .disabled(isFixed)
            switch kind {
            case .server:
                LabeledContent("Scope") { Text("Every database (*.*)").foregroundStyle(.secondary) }
            case .database:
                if dbKind == .postgres || isFixed {
                    LabeledContent("Database") { Text(object?.title ?? tab.connection.defaultDatabase) }
                } else {
                    namePicker("Database", selection: $database, names: databases)
                }
            case .schema, .allTables, .allSequences:
                namePicker(dbKind == .mysql ? "Database" : "Schema", selection: $schema, names: schemas.map(\.name))
            case .table, .sequence:
                namePicker(dbKind == .mysql ? "Database" : "Schema", selection: $schema, names: schemas.map(\.name))
                    .onChange(of: schema) { if !isFixed { table = "" } }
                namePicker(kind == .sequence ? "Sequence" : "Table", selection: $table, names: tableNames)
            }
        } header: {
            Text("On")
        } footer: {
            switch kind {
            case .allTables:
                Text("Applies to the tables and views in the schema now; tables created later aren’t included.")
                    .font(.caption).foregroundStyle(.secondary)
            case .allSequences:
                Text("Applies to the sequences in the schema now. Inserting into tables with serial or identity columns needs USAGE on their sequences.")
                    .font(.caption).foregroundStyle(.secondary)
            case .database where dbKind == .postgres:
                Text("Postgres privileges are per database: connect to another database to manage its privileges.")
                    .font(.caption).foregroundStyle(.secondary)
            default:
                EmptyView()
            }
        }
    }

    private var tableNames: [String] {
        let tables = schemas.first { $0.name == schema }?.tables.map(\.name) ?? []
        return isFixed && !tables.contains(table) ? [table] + tables : tables
    }

    /// A menu of names; a fixed (edited) object shows just its own.
    @ViewBuilder
    private func namePicker(_ title: String, selection: Binding<String>, names: [String]) -> some View {
        if isFixed {
            LabeledContent(title) { Text(selection.wrappedValue) }
        } else {
            Picker(title, selection: selection) {
                Text("Choose…").tag("")
                ForEach(names, id: \.self) { Text($0).tag($0) }
            }
        }
    }

    // MARK: Privileges

    private var privilegesSection: some View {
        Section {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 150), alignment: .leading)], alignment: .leading, spacing: 8) {
                ForEach(available, id: \.self) { privilege in
                    Toggle(privilege, isOn: Binding(
                        get: { after.privileges.contains(privilege) },
                        set: { on in
                            if on {
                                // Keep the server's order.
                                after.privileges = available.filter { after.privileges.contains($0) || $0 == privilege }
                            } else {
                                after.privileges.removeAll { $0 == privilege }
                            }
                        }
                    ))
                    .toggleStyle(.checkbox)
                    .font(.system(size: 12, design: .monospaced))
                }
            }
            .padding(.vertical, 2)
            Toggle(isOn: $after.grantable) {
                Text("With grant option")
                Text("The role may grant these privileges to others.")
            }
        } header: {
            HStack {
                Text("Privileges")
                Spacer()
                Button("All") { after.privileges = available }
                    .disabled(object == nil)
                Button("None") { after.privileges = [] }
                    .disabled(object == nil)
            }
            .buttonStyle(.link)
        }
        .disabled(object == nil)
    }

    @ViewBuilder
    private var sqlSection: some View {
        Section("SQL") {
            switch preview {
            case nil:
                Text("Choose what to grant privileges on.").foregroundStyle(.secondary)
            case .success(let statements)? where statements.isEmpty:
                Text("Nothing changed.").foregroundStyle(.secondary)
            case .success(let statements)?:
                DDLView(sql: statements.map(\.display).joined(separator: ";\n") + ";", fontSize: 11)
                    .listRowInsets(EdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6))
            case .failure(let error)?:
                Text(error.localizedDescription).foregroundStyle(.secondary)
            }
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let error {
                Label {
                    Text(error).textSelection(.enabled).fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red)
                }
                .font(.callout)
            }
            HStack {
                if saving { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Apply") { apply() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(saving || !canApply)
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
        .padding(.top, 4)
    }

    private var canApply: Bool {
        if case .success(let statements)? = preview { !statements.isEmpty } else { false }
    }

    // MARK: Actions

    private func loadObjects() async {
        let driver = model.driver(for: tab.connection)
        do {
            schemas = try await driver.listSchemas()
            if dbKind == .mysql { databases = try await driver.listDatabases() }
            if !isFixed {
                // Start somewhere sensible: the default schema / current database.
                if schema.isEmpty {
                    let preferred = dbKind == .mysql ? tab.connection.defaultDatabase : "public"
                    schema = schemas.first { $0.name == preferred }?.name ?? schemas.first?.name ?? ""
                }
                if database.isEmpty { database = databases.first { $0 == tab.connection.defaultDatabase } ?? databases.first ?? "" }
            }
        } catch {
            self.error = error.localizedDescription
        }
        after = before
        loading = false
    }

    private func apply() {
        guard let change else { return }
        saving = true
        error = nil
        Task {
            do {
                try await model.applyAccess(change, in: tab)
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            saving = false
        }
    }
}
