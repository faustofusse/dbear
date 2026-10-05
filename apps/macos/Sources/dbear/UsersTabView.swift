import DBKit
import SwiftUI

/// "Users & Roles": roles on the left, the selected role's attributes, memberships and
/// privileges on the right. Changes go through sheets that show the SQL before running it.
struct UsersTabView: View {
    @Environment(AppModel.self) private var model
    let tab: UsersTab

    var body: some View {
        Group {
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
                HStack(spacing: 0) {
                    RoleList(tab: tab)
                        .frame(width: 250)
                    Divider()
                    if let role = tab.selected {
                        RoleDetail(tab: tab, role: role)
                    } else {
                        EmptyPlaceholder(text: "No User Selected")
                    }
                }
            }
        }
        .sheet(item: Binding(get: { tab.roleEditor }, set: { tab.roleEditor = $0 })) { request in
            RoleEditorSheet(tab: tab, original: request.original)
        }
        .sheet(item: Binding(get: { tab.privilegeEditor }, set: { tab.privilegeEditor = $0 })) { request in
            PrivilegeEditorSheet(tab: tab, role: request.role, initialObject: request.object)
        }
        .alert(
            "Drop “\(tab.pendingDrop?.reference.title ?? "")”?",
            isPresented: Binding(get: { tab.pendingDrop != nil }, set: { if !$0 { tab.pendingDrop = nil } }),
            presenting: tab.pendingDrop
        ) { role in
            Button("Drop", role: .destructive) { model.confirmAccess(.dropRole(role.reference), in: tab) }
            Button("Cancel", role: .cancel) {}
        } message: { role in
            Text(dropMessage(role))
        }
        .alert(
            "Revoke All Privileges?",
            isPresented: Binding(get: { tab.pendingRevoke != nil }, set: { if !$0 { tab.pendingRevoke = nil } }),
            presenting: tab.pendingRevoke
        ) { grant in
            Button("Revoke", role: .destructive) {
                guard let role = tab.selectedRole else { return }
                model.confirmAccess(.setPrivileges(role: role, object: grant.object, before: grant.privileges, after: PrivilegeSet()), in: tab)
            }
            Button("Cancel", role: .cancel) {}
        } message: { grant in
            Text("\(tab.selectedRole?.title ?? "The role") loses \(grant.privileges.privileges.joined(separator: ", ")) on \(grant.object.title).")
        }
        .alert(
            "Couldn’t Change Privileges",
            isPresented: Binding(get: { tab.actionError != nil }, set: { if !$0 { tab.actionError = nil } })
        ) {
            Button("OK") {}
        } message: {
            Text(tab.actionError ?? "")
        }
    }

    private func dropMessage(_ role: Role) -> String {
        let sql = (try? model.previewAccess(.dropRole(role.reference), in: tab))?.map(\.display).joined(separator: "\n") ?? ""
        let note = tab.connection.kind == .postgres
            ? "\n\nA role that owns objects or holds privileges can’t be dropped until they’re reassigned or revoked."
            : ""
        return sql + note
    }
}

// MARK: - Role list

private struct RoleList: View {
    @Environment(AppModel.self) private var model
    let tab: UsersTab
    @FocusState private var focused: Bool

    var body: some View {
        @Bindable var tab = tab
        VStack(spacing: 0) {
            SearchField(text: $tab.search, prompt: "Filter")
                .padding(.horizontal, 10)
                .padding(.vertical, 8)
            List {
                ForEach(tab.visibleRoles) { role in
                    RoleRow(role: role, showsHost: tab.features.hosts)
                        .mailSelection(role.reference == tab.selectedRole) {
                            focused = true
                            model.selectRole(role.reference, in: tab)
                        }
                        .contextMenu { menu(for: role) }
                }
            }
            .listStyle(.sidebar)
            .scrollContentBackground(.hidden)
            .arrowKeySelection(ids: tab.visibleRoles.map(\.reference), selected: tab.selectedRole, focus: $focused) {
                model.selectRole($0, in: tab)
            }
            .overlay {
                if tab.visibleRoles.isEmpty {
                    Text(tab.search.isEmpty ? "No Users" : "No Matches")
                        .foregroundStyle(.secondary)
                }
            }
            BottomBar {
                Button {
                    tab.roleEditor = RoleEditorRequest(original: nil)
                } label: {
                    Image(systemName: "plus").frame(width: 20, height: 20)
                }
                .help(tab.connection.kind == .mysql ? "New User" : "New Role")
                Button {
                    tab.pendingDrop = tab.selected
                } label: {
                    Image(systemName: "minus").frame(width: 20, height: 20)
                }
                .disabled(tab.selected == nil || tab.selected?.isSystem == true)
                .help("Drop the selected role")
                Spacer()
                Text(countText)
                Menu {
                    Toggle("Show Built-in Roles", isOn: $tab.showsSystemRoles)
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("List options")
            }
            .buttonStyle(.borderless)
        }
    }

    private var countText: String {
        let n = tab.visibleRoles.count
        return n == 1 ? "1 role" : "\(n.formatted()) roles"
    }

    @ViewBuilder
    private func menu(for role: Role) -> some View {
        Button("Edit…") { tab.roleEditor = RoleEditorRequest(original: role) }
        Button("Grant Privileges…") {
            model.selectRole(role.reference, in: tab)
            tab.privilegeEditor = PrivilegeEditorRequest(role: role.reference, object: nil)
        }
        Button("Copy Name") {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(role.name, forType: .string)
        }
        Divider()
        Button("Drop…", role: .destructive) { tab.pendingDrop = role }
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
                        RoleChips(roles: role.memberOf, empty: "Not a member of any role") { model.selectRole($0, in: tab) }
                    }
                    let members = tab.members(of: role)
                    if !members.isEmpty {
                        section("Members", count: members.count) {
                            RoleChips(roles: members.map(\.reference), empty: "") { model.selectRole($0, in: tab) }
                        }
                    }
                }
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
            Button("Edit…") { tab.roleEditor = RoleEditorRequest(original: role) }
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
                Button("Grant…") { tab.privilegeEditor = PrivilegeEditorRequest(role: role.reference, object: nil) }
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
                    tab.privilegeEditor = PrivilegeEditorRequest(role: role.reference, object: grant.object)
                } revoke: { grant in
                    tab.pendingRevoke = grant
                }
            }
        }
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
