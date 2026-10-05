import DBKit
import SwiftUI

/// The users tab: the role selected in the middle column (users mode), with its attributes,
/// memberships and privileges. Changes go through sheets that show the SQL before running it.
struct UsersTabView: View {
    @Environment(AppModel.self) private var model
    let tab: UsersTab

    var body: some View {
        switch tab.roles {
        case .idle, .loading:
            ProgressView().controlSize(.small)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            ContentUnavailableView {
                Label("Couldn’t List Users", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message).textSelection(.enabled)
            } actions: {
                Button("Try Again") { Task { await model.loadRoles(tab) } }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .loaded:
            if let role = tab.selected {
                RoleDetail(tab: tab, role: role)
            } else {
                EmptyPlaceholder(text: "No User Selected")
            }
        }
    }
}

// MARK: - Sheets & confirmations

extension View {
    /// Role and privilege sheets, drop / revoke confirmations: on the window, so they open from
    /// the middle column's list as well as from the users tab.
    func usersSheets(_ model: AppModel) -> some View {
        modifier(UsersSheets(model: model))
    }
}

private struct UsersSheets: ViewModifier {
    @Bindable var model: AppModel

    func body(content: Content) -> some View {
        content
            .sheet(item: $model.roleEditor) { request in
                RoleEditorSheet(tab: request.state, original: request.original)
            }
            .sheet(item: $model.privilegeEditor) { request in
                PrivilegeEditorSheet(tab: request.state, role: request.role, initialObject: request.object)
            }
            .alert(
                "Drop “\(model.pendingRoleDrop?.role.reference.title ?? "")”?",
                isPresented: Binding(get: { model.pendingRoleDrop != nil }, set: { if !$0 { model.pendingRoleDrop = nil } }),
                presenting: model.pendingRoleDrop
            ) { pending in
                Button("Drop", role: .destructive) { model.confirmAccess(.dropRole(pending.role.reference), in: pending.state) }
                Button("Cancel", role: .cancel) {}
            } message: { pending in
                Text(dropMessage(pending))
            }
            .alert(
                "Revoke All Privileges?",
                isPresented: Binding(get: { model.pendingRevoke != nil }, set: { if !$0 { model.pendingRevoke = nil } }),
                presenting: model.pendingRevoke
            ) { pending in
                Button("Revoke", role: .destructive) {
                    let change = AccessChange.setPrivileges(
                        role: pending.role, object: pending.grant.object, before: pending.grant.privileges, after: PrivilegeSet())
                    model.confirmAccess(change, in: pending.state)
                }
                Button("Cancel", role: .cancel) {}
            } message: { pending in
                Text("\(pending.role.title) loses \(pending.grant.privileges.privileges.joined(separator: ", ")) on \(pending.grant.object.title).")
            }
            .alert(
                "Couldn’t Change Privileges",
                isPresented: Binding(get: { model.accessError != nil }, set: { if !$0 { model.accessError = nil } })
            ) {
                Button("OK") {}
            } message: {
                Text(model.accessError ?? "")
            }
    }

    private func dropMessage(_ pending: PendingRoleDrop) -> String {
        let sql = (try? model.previewAccess(.dropRole(pending.role.reference), in: pending.state))?
            .map(\.display).joined(separator: "\n") ?? ""
        let note = pending.state.connection.kind == .postgres
            ? "\n\nA role that owns objects or holds privileges can’t be dropped until they’re reassigned or revoked."
            : ""
        return sql + note
    }
}

// MARK: - Role list (middle column, users mode)

struct UsersList: View {
    @Environment(AppModel.self) private var model
    let state: UsersTab
    @FocusState private var focused: Bool

    var body: some View {
        @Bindable var state = state
        switch state.roles {
        case .idle, .loading:
            ProgressView().controlSize(.small)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            ContentUnavailableView {
                Label("Couldn’t List Users", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message).textSelection(.enabled)
            } actions: {
                Button("Try Again") { Task { await model.loadRoles(state) } }
            }
        case .loaded:
            VStack(spacing: 0) {
                SearchField(text: $state.search, prompt: "Filter")
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                List {
                    ForEach(state.visibleRoles) { role in
                        RoleRow(role: role, showsHost: state.features.hosts)
                            .mailSelection(isShown(role)) {
                                focused = true
                                model.showRole(role.reference, in: state)
                            }
                            .contextMenu { menu(for: role) }
                    }
                }
                .listStyle(.sidebar)
                .scrollContentBackground(.hidden)
                .arrowKeySelection(ids: state.visibleRoles.map(\.reference), selected: state.selectedRole, focus: $focused) {
                    model.showRole($0, in: state)
                }
                .overlay {
                    if state.visibleRoles.isEmpty {
                        Text(state.search.isEmpty ? "No Users" : "No Matches")
                            .foregroundStyle(.secondary)
                    }
                }
                .contextMenu { newRoleButton }
            }
        }
    }

    /// Highlighted like a table: the role the users tab shows, while that tab is the active one.
    private func isShown(_ role: Role) -> Bool {
        role.reference == state.selectedRole && model.activeTabID == state.id
    }

    private var newRoleButton: some View {
        Button(state.connection.kind == .mysql ? "New User…" : "New Role…") {
            model.roleEditor = RoleEditorRequest(state: state, original: nil)
        }
    }

    @ViewBuilder
    private func menu(for role: Role) -> some View {
        Button("Edit…") { model.roleEditor = RoleEditorRequest(state: state, original: role) }
        Button("Grant Privileges…") {
            model.showRole(role.reference, in: state)
            model.privilegeEditor = PrivilegeEditorRequest(state: state, role: role.reference, object: nil)
        }
        Button("Copy Name") {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(role.name, forType: .string)
        }
        Divider()
        newRoleButton
        Button("Drop…", role: .destructive) { model.pendingRoleDrop = PendingRoleDrop(state: state, role: role) }
            .disabled(role.isSystem)
    }
}

private struct RoleRow: View {
    let role: Role
    let showsHost: Bool

    var body: some View {
        Label {
            HStack(spacing: 6) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(role.name).lineLimit(1).truncationMode(.middle)
                    if showsHost, let host = role.host {
                        Text(host).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                    }
                }
                Spacer(minLength: 4)
                if role.isSuperuser {
                    Image(systemName: "bolt.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(.orange)
                        .help("Superuser")
                }
            }
        } icon: {
            Image(systemName: role.roleSymbol)
        }
        .opacity(role.isSystem ? 0.6 : 1)
        .padding(.vertical, showsHost ? 2 : 0)
    }
}

extension Role {
    /// A person for logins, people for group roles.
    var roleSymbol: String { canLogin ? "person" : "person.2" }

    /// "Can log in · Superuser", "Group role"…
    var kindDescription: String {
        var parts = [canLogin ? "Can log in" : (host == nil ? "Group role (no login)" : "Locked account")]
        if isSuperuser { parts.append("Superuser") }
        if isSystem { parts.append("Built in") }
        return parts.joined(separator: " · ")
    }
}

/// A rounded filter field (SwiftUI's `.searchable` only lives in toolbars).
private struct SearchField: View {
    @Binding var text: String
    let prompt: String

    var body: some View {
        HStack(spacing: 4) {
            Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
            TextField(prompt, text: $text)
                .textFieldStyle(.plain)
            if !text.isEmpty {
                Button { text = "" } label: {
                    Image(systemName: "xmark.circle.fill").foregroundStyle(.tertiary)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .background(RoundedRectangle(cornerRadius: 7).fill(.primary.opacity(0.06)))
    }
}

// MARK: - Role detail

private struct RoleDetail: View {
    @Environment(AppModel.self) private var model
    let tab: UsersTab
    let role: Role

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                header
                section("Attributes") { attributes }
                if tab.features.membership {
                    section("Member Of", count: role.memberOf.count) {
                        RoleChips(roles: role.memberOf, empty: "Not a member of any role") { model.showRole($0, in: tab) }
                    }
                    let members = tab.members(of: role)
                    if !members.isEmpty {
                        section("Members", count: members.count) {
                            RoleChips(roles: members.map(\.reference), empty: "") { model.showRole($0, in: tab) }
                        }
                    }
                }
                if tab.connection.kind == .postgres || tab.connection.kind == .mysql { databaseAccess }
                privileges
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .textSelection(.enabled)
        }
        .scrollContentBackground(.hidden)
    }

    private var header: some View {
        HStack(spacing: 12) {
            Image(systemName: role.roleSymbol + ".fill")
                .font(.system(size: 20))
                .foregroundStyle(.white)
                .frame(width: 44, height: 44)
                .background(role.isSuperuser ? AnyShapeStyle(.orange) : AnyShapeStyle(.tint), in: Circle())
            VStack(alignment: .leading, spacing: 2) {
                Text(role.reference.title).font(.title2.weight(.semibold))
                Text(role.kindDescription).foregroundStyle(.secondary)
            }
            Spacer()
            Button("Edit…") { model.roleEditor = RoleEditorRequest(state: tab, original: role) }
                .help("Change name, password, attributes and memberships")
        }
    }

    // MARK: Attributes

    private var attributes: some View {
        let f = tab.features
        return InfoGrid {
            InfoRow("Can log in") { YesNo(role.canLogin) }
            if f.superuser { InfoRow("Superuser") { YesNo(role.isSuperuser) } }
            if !f.superuser && role.isSuperuser { InfoRow("SUPER privilege") { YesNo(true) } }
            if f.createDB { InfoRow("Create databases") { YesNo(role.canCreateDB) } }
            if f.createRole { InfoRow("Create roles") { YesNo(role.canCreateRole) } }
            if f.connectionLimit {
                InfoRow("Connection limit") {
                    Text(role.connectionLimit.map { $0.formatted() } ?? "Unlimited")
                        .foregroundStyle(role.connectionLimit == nil ? .secondary : .primary)
                }
            }
            if f.validUntil {
                InfoRow("Password expires") {
                    Text(role.validUntil ?? "Never").foregroundStyle(role.validUntil == nil ? .secondary : .primary)
                }
            }
            if let comment = role.comment, !comment.isEmpty {
                InfoRow("Comment") { Text(comment) }
            }
        }
    }

    // MARK: Privileges

    @ViewBuilder
    private var privileges: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text("Privileges").font(.headline)
                if let grants = tab.grants.value { Text(grants.count.formatted()).foregroundStyle(.secondary) }
                if tab.features.grantsPerDatabase {
                    Text("in \(tab.connection.defaultDatabase)").foregroundStyle(.secondary)
                }
                Spacer()
                Button("Grant…") { model.privilegeEditor = PrivilegeEditorRequest(state: tab, role: role.reference, object: nil) }
                    .help("Grant privileges on a database, schema or table")
            }
            if role.isSuperuser && tab.connection.kind == .postgres {
                Label("Superusers bypass all privilege checks.", systemImage: "bolt.fill")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            switch tab.grants {
            case .idle, .loading:
                ProgressView().controlSize(.small).padding(.vertical, 8)
            case .failed(let message):
                Label(message, systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.secondary)
            case .loaded(let grants) where grants.isEmpty:
                Text(emptyPrivilegesText)
                    .foregroundStyle(.secondary)
            case .loaded(let grants):
                PrivilegesGrid(grants: grants) { grant in
                    model.privilegeEditor = PrivilegeEditorRequest(state: tab, role: role.reference, object: grant.object)
                } revoke: { grant in
                    model.pendingRevoke = PendingRevoke(state: tab, role: role.reference, grant: grant)
                }
            }
        }
    }

    // MARK: Database access

    /// Databases of the server this role has privileges on (or owns). Edited in the role sheet.
    @ViewBuilder
    private var databaseAccess: some View {
        let databases = tab.databaseAccess.value ?? []
        let granted = databases.filter { $0.isOwner || !$0.privileges.isEmpty }
        section("Database Access", count: tab.databaseAccess.value == nil ? nil : granted.count) {
            switch tab.databaseAccess {
            case .idle, .loading:
                ProgressView().controlSize(.small)
            case .failed(let message):
                Label(message, systemImage: "exclamationmark.triangle").foregroundStyle(.secondary)
            case .loaded:
                VStack(alignment: .leading, spacing: 8) {
                    if granted.isEmpty {
                        Text("No privileges granted on any database directly.").foregroundStyle(.secondary)
                    } else {
                        InfoGrid {
                            ForEach(granted) { database in
                                InfoRow(database.database) { Text(accessDescription(database)) }
                            }
                        }
                    }
                    let open = databases.filter(\.everyoneCanConnect).map(\.database)
                    if !open.isEmpty {
                        Text("Any role can connect to \(open.formatted(.list(type: .and))) (granted to PUBLIC).")
                            .font(.callout)
                            .foregroundStyle(.secondary)
                    }
                }
            }
        }
    }

    private func accessDescription(_ database: DatabaseAccess) -> String {
        if database.isOwner { return "Owner" }
        let level = tab.databaseLevels[database.database] ?? database.level
        guard level == .custom else { return level.title }
        let privileges = database.privileges.privileges.joined(separator: ", ")
        return "Custom: \(privileges) on the database" + (database.privileges.grantable ? " (with grant option)" : "")
    }

    private var emptyPrivilegesText: String {
        let inherited = role.memberOf.isEmpty ? "" : " It may still inherit privileges from the roles it’s a member of."
        return "No privileges granted directly." + inherited
    }

    private func section<Content: View>(_ title: String, count: Int? = nil, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(title).font(.headline)
                if let count { Text(count.formatted()).foregroundStyle(.secondary) }
            }
            content()
        }
    }
}

/// Role names as small capsules; clicking one selects it.
private struct RoleChips: View {
    let roles: [RoleRef]
    let empty: String
    let select: (RoleRef) -> Void

    var body: some View {
        if roles.isEmpty {
            Text(empty).foregroundStyle(.secondary)
        } else {
            FlowLayout(spacing: 6) {
                ForEach(roles, id: \.self) { role in
                    Button { select(role) } label: {
                        Label(role.title, systemImage: "person.2")
                            .font(.callout)
                            .padding(.horizontal, 9)
                            .padding(.vertical, 4)
                            .background(Capsule().fill(.primary.opacity(0.07)))
                    }
                    .buttonStyle(.plain)
                    .help("Show \(role.title)")
                }
            }
        }
    }
}

/// Lays children out left to right, wrapping to new lines.
private struct FlowLayout: Layout {
    var spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(subviews, width: proposal.width ?? .infinity)
        let width = rows.map { $0.width }.max() ?? 0
        let height = rows.map(\.height).reduce(0, +) + spacing * CGFloat(max(0, rows.count - 1))
        return CGSize(width: width, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        var y = bounds.minY
        for row in arrange(subviews, width: bounds.width) {
            var x = bounds.minX
            for index in row.indices {
                let size = subviews[index].sizeThatFits(.unspecified)
                subviews[index].place(at: CGPoint(x: x, y: y), proposal: ProposedViewSize(size))
                x += size.width + spacing
            }
            y += row.height + spacing
        }
    }

    private struct Line {
        var indices: [Int] = []
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func arrange(_ subviews: Subviews, width: CGFloat) -> [Line] {
        var lines = [Line()]
        for (index, subview) in subviews.enumerated() {
            let size = subview.sizeThatFits(.unspecified)
            if !lines[lines.count - 1].indices.isEmpty, lines[lines.count - 1].width + spacing + size.width > width {
                lines.append(Line())
            }
            var line = lines[lines.count - 1]
            line.width += (line.indices.isEmpty ? 0 : spacing) + size.width
            line.height = max(line.height, size.height)
            line.indices.append(index)
            lines[lines.count - 1] = line
        }
        return lines
    }
}

// MARK: - Grids

/// Label / value pairs in a light rounded box, like the structure view.
private struct InfoGrid<Rows: View>: View {
    @ViewBuilder var rows: Rows

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 24, verticalSpacing: 8) {
            rows
        }
        .font(.system(size: 13))
        .padding(12)
        .frame(maxWidth: 520, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.03)))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.primary.opacity(0.08), lineWidth: 0.5))
    }
}

private struct InfoRow<Value: View>: View {
    let title: String
    @ViewBuilder var value: Value

    init(_ title: String, @ViewBuilder value: () -> Value) {
        self.title = title
        self.value = value()
    }

    var body: some View {
        GridRow {
            Text(title).foregroundStyle(.secondary)
            value
        }
    }
}

private struct YesNo: View {
    let value: Bool
    init(_ value: Bool) { self.value = value }

    var body: some View {
        if value {
            Label("Yes", systemImage: "checkmark").labelStyle(.titleAndIcon)
        } else {
            Text("No").foregroundStyle(.secondary)
        }
    }
}

/// One row per object: what it is, the privileges, and whether they can be granted on.
private struct PrivilegesGrid: View {
    let grants: [ObjectPrivileges]
    let edit: (ObjectPrivileges) -> Void
    let revoke: (ObjectPrivileges) -> Void

    var body: some View {
        Grid(alignment: .leading, horizontalSpacing: 18, verticalSpacing: 7) {
            GridRow {
                Color.clear.frame(width: 14, height: 1)
                header("Object")
                header("Type")
                header("Privileges")
                header("Grant Option")
                Color.clear.frame(width: 1, height: 1)
            }
            Divider()
            ForEach(Array(grants.enumerated()), id: \.element.id) { index, grant in
                if index > 0 { Divider().opacity(0.5) }
                GridRow {
                    Image(systemName: grant.object.systemImage)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .frame(width: 14)
                    Text(grant.object.title)
                    Text(grant.object.kind.title).foregroundStyle(.secondary)
                    privilegeList(grant.privileges.privileges)
                    Group {
                        if grant.privileges.grantable {
                            Image(systemName: "checkmark").foregroundStyle(.secondary)
                        } else {
                            Color.clear.frame(width: 1, height: 1)
                        }
                    }
                    .gridColumnAlignment(.center)
                    HStack(spacing: 10) {
                        Button("Edit") { edit(grant) }
                            .buttonStyle(.link)
                        Button("Revoke") { revoke(grant) }
                            .buttonStyle(.link)
                            .foregroundStyle(.red)
                    }
                    .font(.callout)
                }
                .contextMenu {
                    Button("Edit Privileges…") { edit(grant) }
                    Button("Revoke All", role: .destructive) { revoke(grant) }
                }
            }
        }
        .font(.system(size: 13))
        .padding(12)
        .background(RoundedRectangle(cornerRadius: 8).fill(.primary.opacity(0.03)))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.primary.opacity(0.08), lineWidth: 0.5))
    }

    private func header(_ title: String) -> some View {
        Text(title)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(.secondary)
    }

    /// Long lists (MySQL's root holds ~80 server privileges) show the first few; all in the tooltip.
    private func privilegeList(_ privileges: [String]) -> some View {
        let limit = 12
        let shown = privileges.prefix(limit).joined(separator: ", ")
        let more = privileges.count - limit
        return (Text(shown) + Text(more > 0 ? "  +\(more) more" : "").foregroundStyle(.secondary))
            .font(.system(size: 12, design: .monospaced))
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: 360, alignment: .leading)
            .help(more > 0 ? privileges.joined(separator: ", ") : "")
    }
}
