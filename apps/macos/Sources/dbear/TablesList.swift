import DBKit
import SwiftUI

struct TablesList: View {
    @Environment(AppModel.self) private var model
    @State private var tablesOnly = false
    /// Folded schema sections, remembered per database so switching connections keeps them.
    @State private var collapsedByTarget: [DriverKey: Set<String>] = [:]
    private var collapsed: Set<String> {
        get { model.selectedTarget.flatMap { collapsedByTarget[$0.driverKey] } ?? [] }
        nonmutating set { if let key = model.selectedTarget?.driverKey { collapsedByTarget[key] = newValue } }
    }
    @FocusState private var focused: Bool
    /// The column's width, to size the database title menu like the native title.
    @State private var width: CGFloat = 300

    var body: some View {
        content
            .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { width = $0 }
            .navigationTitle(title)
            .navigationSubtitle(subtitle)
            // With several databases the title becomes a menu (`toolbarTitleMenu` shows no indicator
            // on macOS, so nobody would find it): same title and subtitle, plus a visible ▾.
            .toolbar(removing: hasDatabaseMenu ? .title : nil)
            .toolbar {
                if hasDatabaseMenu {
                    ToolbarItem(placement: .navigation) {
                        DatabaseTitle(
                            title: title, subtitle: subtitle, help: model.selectedTarget?.summary,
                            // What the filter and ••• buttons leave free, as the native title gets.
                            maxWidth: max(80, width - 110),
                            popUpRequested: model.databaseMenuRequest != nil
                                && model.databaseMenuRequest == model.selectedConnectionID,
                            popUpItems: databaseMenuItems,
                            onPopUp: { model.databaseMenuRequest = nil }
                        ) { databaseMenu }
                    }
                    .sharedBackgroundIfAvailable(hidden: true)
                    // Like the native title, take the free space so the buttons stay at the trailing edge.
                    if #available(macOS 26.0, *) {
                        ToolbarSpacer(.flexible)
                    }
                }
                ToolbarItemGroup {
                    Toggle(isOn: $tablesOnly) {
                        Label("Filter", systemImage: "line.3.horizontal.decrease")
                    }
                    .help(tablesOnly ? "Showing tables only" : "Filter: tables only")

                    Menu {
                        Button("Refresh") { Task { await model.loadSchemas(refresh: true) } }
                        if let connection = model.selectedConnection {
                            Divider()
                            Button("Dump Database…") { model.requestDump(of: connection) }
                            Button("Restore from File…") { model.requestRestore(into: connection) }
                            if model.canManageUsers(connection) {
                                Divider()
                                Button("Users & Roles") { model.openUsers() }
                            }
                        }
                        // A single MySQL database has no sections to fold.
                        if singleDatabase(in: model.schemas.value ?? []) == nil {
                            Divider()
                            Button("Expand All") { collapsed.removeAll() }
                            Button("Collapse All") {
                                collapsed = Set(model.schemas.value?.map(\.name) ?? [])
                            }
                        }
                    } label: {
                        Label("More", systemImage: "ellipsis")
                    }
                    .menuIndicator(.hidden)
                }
            }
            .task(id: model.selectedTarget?.driverKey) {
                await model.loadSchemas()
            }
    }

    /// The database when the title is a database menu, else the connection's name.
    private var title: String {
        guard let target = model.selectedTarget else { return "Tables" }
        if let connection = model.selectedConnection, model.databases(of: connection) != nil {
            return target.defaultDatabase
        }
        return model.displayName(of: target)
    }

    private var hasDatabaseMenu: Bool {
        model.selectedConnection.flatMap { model.databases(of: $0) } != nil
    }

    /// Title menu of a connection that lists its server's databases: pick the one to browse.
    @ViewBuilder
    private var databaseMenu: some View {
        if let connection = model.selectedConnection, let databases = model.databases(of: connection) {
            Picker("Database", selection: Binding(
                get: { model.selectedTarget?.defaultDatabase ?? connection.defaultDatabase },
                set: { model.select(connection.id, database: $0) }
            )) {
                ForEach(databases, id: \.self) { database in
                    Text(database == connection.defaultDatabase ? "\(database) (default)" : database).tag(database)
                }
            }
            .pickerStyle(.inline)
            .labelsHidden()
            Divider()
            Button("Refresh Databases") { Task { await model.loadDatabases(connection) } }
        }
    }

    /// `databaseMenu` as AppKit items, for opening it without a click (SwiftUI menus can't be).
    private func databaseMenuItems() -> [PopUpMenuItem] {
        guard let connection = model.selectedConnection, let databases = model.databases(of: connection) else { return [] }
        let current = model.selectedTarget?.defaultDatabase ?? connection.defaultDatabase
        let picks = databases.map { database in
            PopUpMenuItem.action(
                database == connection.defaultDatabase ? "\(database) (default)" : database,
                checked: database == current
            ) { model.select(connection.id, database: database) }
        }
        return picks + [.separator, .action("Refresh Databases") { Task { await model.loadDatabases(connection) } }]
    }

    /// The one database a MySQL connection browses (its only "schema"), if that's what's shown.
    private func singleDatabase(in schemas: [Schema]) -> Schema? {
        guard model.selectedTarget?.kind == .mysql, schemas.count == 1 else { return nil }
        return schemas[0]
    }

    private func tableRow(_ table: TableInfo) -> some View {
        TableRow(table: table)
            .id(table.id)
            .mailSelection(table.id == model.selectedTableID) {
                model.openTable(table, pinned: false)
                focused = true
            }
            // Double-click keeps the tab open instead of previewing.
            .simultaneousGesture(TapGesture(count: 2).onEnded {
                model.openTable(table, pinned: true)
            })
            .contextMenu {
                Button("Open in New Tab") { model.openTable(table, pinned: true) }
                if let connection = model.selectedConnection {
                    Button("Dump “\(table.name)”…") { model.requestDump(of: connection, preset: .table(table)) }
                }
            }
    }

    private var subtitle: String {
        guard let schemas = model.schemas.value else {
            // Not the connection summary: host names are long, and the title jumped while loading.
            return model.schemas.isLoading ? "Loading…" : model.selectedTarget?.summary ?? ""
        }
        if tablesOnly { return "Filter by: Tables only" }
        let tables = schemas.reduce(0) { $0 + $1.tables.count }
        func count(_ n: Int, _ noun: String) -> String { "\(n) \(noun)\(n == 1 ? "" : "s")" }
        // A MySQL connection browses one database: its only section is the database itself.
        if model.selectedTarget?.kind == .mysql, schemas.count == 1 { return count(tables, "table") }
        // Without a database, MySQL lists every database as a section; SQLite's are attached databases.
        let section = [.postgres, .sqlServer].contains(model.selectedTarget?.kind) ? "schema" : "database"
        return "\(count(schemas.count, section)), \(count(tables, "table"))"
    }

    @ViewBuilder
    private var content: some View {
        if model.selectedConnection == nil {
            Color.clear
        } else {
            switch model.schemas {
            case .idle, .loading:
                ProgressView().controlSize(.small)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .failed(let message):
                ContentUnavailableView {
                    Label("Couldn’t Connect", systemImage: "exclamationmark.triangle")
                } description: {
                    Text(message)
                } actions: {
                    Button("Try Again") { Task { await model.loadSchemas(refresh: true) } }
                }
            case .loaded(let schemas):
                ScrollViewReader { proxy in
                List {
                    if let database = singleDatabase(in: schemas) {
                        // A MySQL database has no schemas: its tables, without a header repeating the title.
                        ForEach(visibleTables(in: database)) { table in tableRow(table) }
                    } else {
                        ForEach(schemas) { schema in
                            Section(isExpanded: expansion(for: schema.name)) {
                                ForEach(visibleTables(in: schema)) { table in tableRow(table) }
                            } header: {
                                Text(schema.name)
                                    .contextMenu {
                                        if let connection = model.selectedConnection {
                                            Button("Dump Schema “\(schema.name)”…") {
                                                model.requestDump(of: connection, preset: .schema(schema.name))
                                            }
                                        }
                                    }
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
                .scrollContentBackground(.hidden)
                .preparesListRows(PreparedRowsToken(schemas: schemas, tablesOnly: tablesOnly))
                .arrowKeySelection(
                    ids: singleDatabase(in: schemas).map { visibleTables(in: $0).map(\.id) }
                        ?? schemas.filter { !collapsed.contains($0.name) }.flatMap { visibleTables(in: $0).map(\.id) },
                    selected: model.selectedTableID,
                    focus: $focused
                ) { id in
                    if let table = model.table(withID: id) { model.openTable(table, pinned: false) }
                }
                .onChange(of: model.selectedTableID) { _, id in
                    if let id { proxy.scrollTo(id) }
                }
                }
            }
        }
    }

    private func visibleTables(in schema: Schema) -> [TableInfo] {
        schema.tables.filter { !tablesOnly || $0.kind == .table }
    }

    private func expansion(for schema: String) -> Binding<Bool> {
        Binding(
            get: { !collapsed.contains(schema) },
            set: { expanded in
                if expanded { collapsed.remove(schema) } else { collapsed.insert(schema) }
            }
        )
    }
}

private struct TableRow: View {
    let table: TableInfo

    var body: some View {
        Label {
            HStack {
                Text(table.name)
                    .lineLimit(1)
                Spacer()
                if let count = table.estimatedRowCount {
                    // Mail-style trailing count, stays gray when selected.
                    Text(count.formatted())
                        .font(.callout)
                        .monospacedDigit()
                        .foregroundStyle(Color.secondary)
                }
            }
        } icon: {
            Image(systemName: table.kind == .view ? "eye" : "tablecells")
        }
        .help("\(table.kind == .view ? "View" : "Table") \(table.id)")
    }
}

/// Looks like the column's native title and subtitle, with a ▾ that opens `menu`.
private struct DatabaseTitle<Items: View>: View {
    let title: String
    let subtitle: String
    let help: String?
    let maxWidth: CGFloat
    /// Pops the menu open without a click; `onPopUp` should reset it.
    let popUpRequested: Bool
    let popUpItems: () -> [PopUpMenuItem]
    let onPopUp: () -> Void
    @ViewBuilder let menu: Items
    @Environment(\.controlActiveState) private var activeState

    var body: some View {
        // Native titles dim when the window isn't key.
        let inactive = activeState == .inactive
        Menu {
            menu
        } label: {
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(title)
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(inactive ? .tertiary : .primary)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .bold))
                        .foregroundStyle(.secondary)
                }
                Text(subtitle)
                    .font(.system(size: 11))
                    .foregroundStyle(inactive ? .tertiary : .secondary)
            }
            // Where the native title starts.
            .padding(.leading, 12)
            .lineLimit(1)
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        // Truncates like the native title instead of growing with long names (it pushed the
        // column's buttons around); the full connection summary is in the tooltip.
        .frame(maxWidth: maxWidth, alignment: .leading)
        // Spans the button, so the menu lands where a click would open it.
        .background(PopUpMenuAnchor(requested: popUpRequested, items: popUpItems, onPopUp: onPopUp))
        .help(help.map { "\($0)\nClick to switch database" } ?? "Click to switch database")
    }
}

extension ToolbarContent {
    /// No shared Liquid Glass capsule behind the item (macOS 26), e.g. so it reads like the plain native title.
    func sharedBackgroundIfAvailable(hidden: Bool) -> some ToolbarContent {
        if #available(macOS 26.0, *) {
            return sharedBackgroundVisibility(hidden ? .hidden : .automatic)
        } else {
            return self
        }
    }
}

/// An item of a menu opened by `PopUpMenuAnchor`.
enum PopUpMenuItem {
    case action(String, checked: Bool = false, perform: () -> Void)
    case separator
}

/// Pops an AppKit menu below itself when `requested`: for opening a menu without a click,
/// which SwiftUI's `Menu` can't do.
private struct PopUpMenuAnchor: NSViewRepresentable {
    let requested: Bool
    let items: () -> [PopUpMenuItem]
    let onPopUp: () -> Void

    func makeNSView(context: Context) -> NSView { FlippedView() }

    func updateNSView(_ view: NSView, context: Context) {
        guard requested, !context.coordinator.pending else { return }
        context.coordinator.pending = true
        let items = items()
        // Not during the SwiftUI update: the reset changes state, and the menu runs a nested event loop.
        DispatchQueue.main.async {
            context.coordinator.pending = false
            onPopUp()
            guard !items.isEmpty, view.window != nil else { return }
            let menu = NSMenu()
            for item in items {
                switch item {
                case .separator:
                    menu.addItem(.separator())
                case .action(let title, let checked, let perform):
                    // `target` is weak and the toolbar may rebuild this view while the menu is open:
                    // the item keeps its own action alive.
                    let action = MenuAction(perform)
                    let menuItem = NSMenuItem(title: title, action: #selector(MenuAction.run), keyEquivalent: "")
                    menuItem.target = action
                    menuItem.representedObject = action
                    menuItem.state = checked ? .on : .off
                    menu.addItem(menuItem)
                }
            }
            // The gap a click on the title menu leaves below the button.
            menu.popUp(positioning: nil, at: NSPoint(x: 0, y: view.bounds.height + 15), in: view)
        }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator: NSObject {
        var pending = false
    }

    private final class MenuAction: NSObject {
        let perform: () -> Void
        init(_ perform: @escaping () -> Void) { self.perform = perform }
        @objc func run() { perform() }
    }

    private final class FlippedView: NSView {
        override var isFlipped: Bool { true }
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
    }
}

/// What the tables column's rows depend on: when it changes, the List's rows are prepared again.
private struct PreparedRowsToken: Hashable {
    let schemas: [Schema]
    let tablesOnly: Bool
}
