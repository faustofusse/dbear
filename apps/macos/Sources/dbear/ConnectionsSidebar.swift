import AppKit
import DBKit
import SwiftUI

struct ConnectionsSidebar: View {
    @Environment(AppModel.self) private var model
    @State private var collapsed: Set<String> = []
    /// Connections whose databases are shown under them in the sidebar.
    @State private var expanded: Set<ConnectionConfig.ID> = []
    @FocusState private var focused: Bool

    var body: some View {
        List {
            ForEach(model.groupedConnections, id: \.group) { group in
                Section(isExpanded: expansion(for: group.group)) {
                    ForEach(group.connections) { connection in
                        connectionRow(connection)
                        // The connection expands to its databases (chevron on the row's trailing edge).
                        if showsDatabases(connection), let databases = sidebarDatabases(of: connection) {
                            ForEach(databases, id: \.self) { database in
                                databaseRow(database, of: connection)
                            }
                        }
                    }
                } header: {
                    Text(group.group.isEmpty ? "Connections" : group.group)
                }
            }
        }
        .listStyle(.sidebar)
        .arrowKeySelection(ids: visibleRows, selected: selectedRow, focus: $focused) { row in
            if let database = row.database {
                model.select(row.connectionID, database: database)
            } else if model.selectedConnectionID != row.connectionID {
                model.select(row.connectionID)
            }
        }
        .onDeleteCommand {
            if let selected = model.selectedConnection { model.pendingDeletion = selected }
        }
        .contextMenu {
            Button("New Connection…") { model.newConnection() }
            Button("Import from DBeaver…") { model.showingImport = true }
        }
        .overlay { emptyState }
        .bottomBar { newConnectionButton }
    }

    private var newConnectionButton: some View {
        Button { model.newConnection() } label: {
            Label("New Connection", systemImage: "plus.circle")
        }
        .buttonStyle(.borderless)
        .foregroundStyle(.secondary)
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 18)
        .padding(.vertical, 12)
        .help("Add a database connection (⇧⌘N)")
    }

    @ViewBuilder
    private var emptyState: some View {
        if let error = model.storeError {
            SidebarMessage(title: "Couldn’t Load Connections", message: error)
        } else if model.connections.isEmpty {
            SidebarMessage(title: "No Connections", message: "Add a database to get started.") {
                Button("New Connection…") { model.newConnection() }
                if DBeaverImport.isInstalled {
                    Button("Import from DBeaver…") { model.showingImport = true }
                }
            }
        }
    }

    private func connectionRow(_ connection: ConnectionConfig) -> some View {
        ConnectionRow(
            connection: connection,
            isOpen: model.openConnections.contains(connection.id),
            failed: model.failedConnections.contains(connection.id),
            isExpanded: sidebarDatabases(of: connection) == nil ? nil : databasesExpanded(connection)
        )
        .mailSelection(selectedRow == SidebarRow(connectionID: connection.id)) {
            // Clicking the selected connection again keeps the database picked for it.
            if model.selectedConnectionID != connection.id { model.select(connection.id) }
            focused = true
        }
        .contextMenu { menu(for: connection) }
    }

    private func databaseRow(_ database: String, of connection: ConnectionConfig) -> some View {
        Label(database, systemImage: "cylinder")
            .lineLimit(1)
            .padding(.leading, 20)
            .help(database == connection.defaultDatabase ? "\(database) (default)" : database)
            .mailSelection(selectedRow == SidebarRow(connectionID: connection.id, database: database)) {
                model.select(connection.id, database: database)
                focused = true
            }
            .contextMenu {
                Button("New SQL Script") {
                    model.select(connection.id, database: database)
                    model.newScript()
                }
            }
    }

    /// The databases listed under a connection: only once listed, and only when there's a choice.
    private func sidebarDatabases(of connection: ConnectionConfig) -> [String]? {
        guard let databases = model.databases(of: connection), databases.count > 1 else { return nil }
        return databases
    }

    private func showsDatabases(_ connection: ConnectionConfig) -> Bool {
        expanded.contains(connection.id) && sidebarDatabases(of: connection) != nil
    }

    private func databasesExpanded(_ connection: ConnectionConfig) -> Binding<Bool> {
        Binding(
            get: { expanded.contains(connection.id) },
            set: { isExpanded in
                withAnimation(.snappy(duration: 0.2)) {
                    if isExpanded { expanded.insert(connection.id) } else { expanded.remove(connection.id) }
                }
            }
        )
    }

    /// The highlighted row: the shown database's row while its connection is expanded, else the connection's.
    private var selectedRow: SidebarRow? {
        guard let connection = model.selectedConnection else { return nil }
        guard showsDatabases(connection), let database = model.selectedTarget?.defaultDatabase,
              sidebarDatabases(of: connection)?.contains(database) == true else {
            return SidebarRow(connectionID: connection.id)
        }
        return SidebarRow(connectionID: connection.id, database: database)
    }

    @ViewBuilder
    private func menu(for connection: ConnectionConfig) -> some View {
        if model.openConnections.contains(connection.id) {
            Button("Disconnect") { Task { await model.disconnect(connection) } }
        } else {
            Button("Connect") { Task { await model.connect(connection) } }
        }
        Button("New SQL Script") {
            model.select(connection.id)
            model.newScript()
        }
        if model.canCreateDatabases(on: connection) {
            Button("New Database…") {
                model.select(connection.id)
                model.requestNewDatabase(on: connection)
            }
        }
        if connection.showAllDatabases, connection.supportsMultipleDatabases, model.openConnections.contains(connection.id) {
            Button("Refresh Databases") { Task { await model.loadDatabases(connection) } }
        }
        Divider()
        Button("Dump Database…") { model.requestDump(of: connection) }
        Button("Restore from File…") { model.requestRestore(into: connection) }
        if model.canManageUsers(connection) {
            Button("Users & Roles") {
                model.select(connection.id)
                model.openUsers()
            }
        }
        Divider()
        Button("Edit…") { model.edit(connection) }
        Button("Duplicate") { model.duplicate(connection) }
        Button("Copy URL") {
            NSPasteboard.general.clearContents()
            var config = connection
            if config.password == nil { config.password = model.savedPassword(config.id) }
            NSPasteboard.general.setString(config.url(includingPassword: true), forType: .string)
        }
        Divider()
        Button("Delete…", role: .destructive) { model.pendingDeletion = connection }
    }

    private var visibleRows: [SidebarRow] {
        model.groupedConnections
            .filter { !collapsed.contains($0.group) }
            .flatMap(\.connections)
            .flatMap { connection -> [SidebarRow] in
                let row = SidebarRow(connectionID: connection.id)
                guard showsDatabases(connection), let databases = sidebarDatabases(of: connection) else { return [row] }
                return [row] + databases.map { SidebarRow(connectionID: connection.id, database: $0) }
            }
    }

    private func expansion(for group: String) -> Binding<Bool> {
        Binding(
            get: { !collapsed.contains(group) },
            set: { if $0 { collapsed.remove(group) } else { collapsed.insert(group) } }
        )
    }
}

/// A row of the sidebar: a connection, or one of its databases.
private struct SidebarRow: Equatable {
    let connectionID: ConnectionConfig.ID
    var database: String? = nil
}

private struct ConnectionRow: View {
    let connection: ConnectionConfig
    let isOpen: Bool
    let failed: Bool
    /// Set when the connection has databases to list under it: drives the trailing chevron.
    var isExpanded: Binding<Bool>?

    var body: some View {
        Label {
            HStack(spacing: 6) {
                Text(connection.name)
                Spacer()
                Group {
                    if failed {
                        Image(systemName: "exclamationmark.triangle")
                            .foregroundStyle(.secondary)
                            .help("Could not connect")
                    } else if isOpen {
                        ConnectedIndicator()
                            .help("Connected")
                            .transition(.scale.combined(with: .opacity))
                    }
                }
                .frame(width: 18)
                if let isExpanded {
                    Button {
                        isExpanded.wrappedValue.toggle()
                    } label: {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(.secondary)
                            .rotationEffect(.degrees(isExpanded.wrappedValue ? 90 : 0))
                            .frame(width: 16, height: 16)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(isExpanded.wrappedValue ? "Hide databases" : "Show databases")
                    .accessibilityLabel(isExpanded.wrappedValue ? "Hide databases" : "Show databases")
                } else {
                    // Same slot on every row, so status dots and warnings line up in one column.
                    Color.clear.frame(width: 16, height: 16)
                }
            }
        } icon: {
            DatabaseKindIcon(kind: connection.kind)
        }
        .help(connection.summary)
        .animation(.spring(duration: 0.3), value: isOpen)
        .accessibilityValue(failed ? "Connection failed" : isOpen ? "Connected" : "Not connected")
    }
}

/// Status light for an open connection: a lit dot with a soft halo that
/// sends out a single ripple when it first appears.
private struct ConnectedIndicator: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var rippling = false

    private let tint = Color(nsColor: .systemGreen)

    var body: some View {
        ZStack {
            Circle()
                .fill(tint.opacity(0.18))
                .frame(width: 13, height: 13)

            Circle()
                .stroke(tint.opacity(rippling ? 0 : 0.6), lineWidth: 1)
                .frame(width: 7, height: 7)
                .scaleEffect(rippling ? 2.4 : 1)

            Circle()
                .fill(tint.gradient)
                .overlay(Circle().strokeBorder(.white.opacity(0.25), lineWidth: 0.5))
                .frame(width: 7, height: 7)
                .shadow(color: tint.opacity(0.7), radius: 2.5)
        }
        .frame(width: 14, height: 14)
        .onAppear {
            guard !reduceMotion else { return }
            withAnimation(.easeOut(duration: 1.1)) { rippling = true }
        }
        .accessibilityHidden(true)
    }
}

/// Compact empty/error state sized for a narrow sidebar.
private struct SidebarMessage<Actions: View>: View {
    let title: String
    let message: String
    @ViewBuilder var actions: Actions

    var body: some View {
        VStack(spacing: 6) {
            Text(title).font(.headline).foregroundStyle(.secondary)
            Text(message)
                .font(.callout)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)
            actions.padding(.top, 6)
        }
        .padding(.horizontal, 20)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension SidebarMessage where Actions == EmptyView {
    init(title: String, message: String) {
        self.init(title: title, message: message) { EmptyView() }
    }
}

private extension View {
    /// Bottom bar that long lists scroll under without the rows showing through:
    /// the system scroll-edge blur on macOS 26, a material background before that.
    @ViewBuilder
    func bottomBar<Bar: View>(@ViewBuilder _ bar: () -> Bar) -> some View {
        if #available(macOS 26.0, *) {
            safeAreaBar(edge: .bottom, spacing: 0, content: bar)
        } else {
            safeAreaInset(edge: .bottom, spacing: 0) { bar().background(.bar) }
        }
    }
}
