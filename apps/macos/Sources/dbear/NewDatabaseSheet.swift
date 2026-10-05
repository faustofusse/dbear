import DBKit
import SwiftUI

/// Which server a "New Database" sheet creates on.
struct NewDatabaseRequest: Identifiable {
    let connection: ConnectionConfig
    var id: ConnectionConfig.ID { connection.id }
}

extension AppModel {
    /// Servers you can create databases on: the ones whose other databases dbear can browse.
    func canCreateDatabases(on connection: ConnectionConfig) -> Bool {
        connection.supportsMultipleDatabases
    }

    func requestNewDatabase(on connection: ConnectionConfig) {
        newDatabaseRequest = NewDatabaseRequest(connection: connection)
    }

    func previewCreateDatabase(named name: String, on connection: ConnectionConfig) -> Result<String, Error> {
        Result { try driver(for: connection).previewCreateDatabase(named: name) }
    }

    /// Creates the database, refreshes the server's list and switches to it (when the connection
    /// browses its server's databases).
    func createDatabase(named name: String, on connection: ConnectionConfig) async throws {
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        try await driver(for: connection).createDatabase(named: name)
        guard connection.showAllDatabases else { return }
        await loadDatabases(connection)
        select(connection.id, database: name)
    }
}

/// "New Database": a name, the statement it runs, Create.
struct NewDatabaseSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let connection: ConnectionConfig

    @State private var name = ""
    @State private var creating = false
    @State private var error: String?
    @FocusState private var nameFocused: Bool

    private var preview: Result<String, Error> {
        model.previewCreateDatabase(named: name, on: connection)
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Form {
                Section {
                    TextField("Name", text: $name, prompt: Text(verbatim: "new_database"))
                        .focused($nameFocused)
                } footer: {
                    Text(note).foregroundStyle(.secondary)
                }
                Section("SQL") {
                    if case .success(let sql) = preview, !name.trimmingCharacters(in: .whitespaces).isEmpty {
                        DDLView(sql: sql + ";", fontSize: 11)
                            .listRowInsets(EdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6))
                    } else {
                        Text("Enter a name.").foregroundStyle(.secondary)
                    }
                }
            }
            .formStyle(.grouped)
            .scrollBounceBehavior(.basedOnSize)
            footer
        }
        .frame(width: 460)
        .fixedSize(horizontal: false, vertical: true)
        .onChange(of: name) { error = nil }
        .onAppear { nameFocused = true }
    }

    private var note: String {
        switch connection.kind {
        case .postgres: "Created from template1 with the server’s encoding and locale, owned by \(connection.user ?? "you")."
        case .mysql: "Uses the server’s default character set and collation."
        default: "Uses the server’s defaults."
        }
    }

    /// "PostgreSQL · localhost:5432": the database lands on the server, not in the shown one.
    private var server: String {
        let address = connection.port.map { "\(connection.host):\($0)" } ?? connection.host
        return "\(connection.kind.displayName) · \(address)"
    }

    private var header: some View {
        HStack(spacing: 12) {
            Image(systemName: "cylinder.split.1x2.fill")
                .font(.system(size: 17))
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(.tint, in: Circle())
            VStack(alignment: .leading, spacing: 2) {
                Text("New Database").font(.headline)
                Text(server)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
        }
        .padding(.horizontal, 20)
        .padding(.top, 20)
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
                if creating { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Create") { create() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(creating || !canCreate)
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
        .padding(.top, 4)
    }

    private var canCreate: Bool {
        guard !name.trimmingCharacters(in: .whitespaces).isEmpty, case .success = preview else { return false }
        return true
    }

    private func create() {
        creating = true
        error = nil
        Task {
            do {
                try await model.createDatabase(named: name, on: connection)
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            creating = false
        }
    }
}
