import DBKit
import Foundation
import Observation

/// "Users & Roles" tab: the server's roles on the left, the selected one's attributes,
/// memberships and privileges on the right. One per connection target (connection + database).
@Observable
@MainActor
final class UsersTab: Identifiable {
    let id = UUID()
    /// The connection pointed at a database: Postgres privileges are listed for that database.
    let connection: ConnectionConfig
    let features: AccessFeatures

    var roles: LoadState<[Role]> = .idle
    var selectedRole: RoleRef?
    /// The selected role's privileges, grouped by object.
    var grants: LoadState<[ObjectPrivileges]> = .idle
    /// Built-in roles (`pg_*`, `mysql.sys`…) are listed too.
    var showsSystemRoles = false
    var search = ""

    /// Sheets: new / edit role, edit privileges on an object.
    var roleEditor: RoleEditorRequest?
    var privilegeEditor: PrivilegeEditorRequest?
    /// Waiting for confirmation.
    var pendingDrop: Role?
    var pendingRevoke: ObjectPrivileges?
    /// A drop or revoke failed (shown in an alert).
    var actionError: String?

    init(connection: ConnectionConfig, features: AccessFeatures) {
        self.connection = connection
        self.features = features
    }

    var selected: Role? {
        guard let selectedRole else { return nil }
        return roles.value?.first { $0.reference == selectedRole }
    }

    /// The list as shown: system roles only when asked for (or selected), filtered by `search`.
    var visibleRoles: [Role] {
        let all = roles.value ?? []
        let query = search.trimmingCharacters(in: .whitespaces)
        return all.filter { role in
            (showsSystemRoles || !role.isSystem || role.reference == selectedRole)
                && (query.isEmpty || role.reference.title.localizedCaseInsensitiveContains(query))
        }
    }

    /// Roles that are members of `role` (inherit its privileges).
    func members(of role: Role) -> [Role] {
        (roles.value ?? []).filter { $0.memberOf.contains(role.reference) }
    }

    /// Roles `role` could become a member of: everything but itself.
    func possibleParents(of role: RoleRef?) -> [Role] {
        (roles.value ?? []).filter { $0.reference != role }
    }

    /// Privileges a role holds on `object`, as listed.
    func privileges(on object: GrantObject) -> PrivilegeSet {
        grants.value?.first { $0.object == object }?.privileges ?? PrivilegeSet()
    }
}

struct RoleEditorRequest: Identifiable {
    let id = UUID()
    /// `nil`: a new role.
    let original: Role?
}

struct PrivilegeEditorRequest: Identifiable {
    let id = UUID()
    let role: RoleRef
    /// The object to edit; `nil` to pick one (new grant).
    let object: GrantObject?
}

extension AppModel {
    var activeUsersTab: UsersTab? {
        if case .users(let t) = activeTab { t } else { nil }
    }

    /// Whether `connection`'s users can be managed here.
    func canManageUsers(_ connection: ConnectionConfig) -> Bool {
        Access.features(connection.kind) != nil
    }

    /// Opens (or shows) the Users & Roles tab for the selected connection and database.
    func openUsers() {
        guard let connection = selectedTarget, let features = Access.features(connection.kind) else { return }
        if let existing = tabs.first(where: {
            if case .users(let u) = $0 { u.connection.driverKey == connection.driverKey } else { false }
        }) {
            activeTabID = existing.id
            return
        }
        let tab = UsersTab(connection: connection, features: features)
        tabs.insert(.users(tab), at: (tabs.firstIndex { $0.id == activeTabID }).map { $0 + 1 } ?? tabs.count)
        activeTabID = tab.id
        Task { await loadRoles(tab) }
    }

    /// (Re)lists roles, keeping the selection if the role still exists, then its privileges.
    func loadRoles(_ tab: UsersTab, select: RoleRef? = nil) async {
        if tab.roles.value == nil { tab.roles = .loading }
        do {
            let roles = try await driver(for: tab.connection).listRoles()
            tab.roles = .loaded(roles)
            let wanted = select ?? tab.selectedRole
            if let wanted, roles.contains(where: { $0.reference == wanted }) {
                tab.selectedRole = wanted
            } else {
                // First the role we're connected as, else the first ordinary role.
                let me = tab.connection.user
                tab.selectedRole = (roles.first { $0.name == me && !$0.isSystem } ?? roles.first { !$0.isSystem } ?? roles.first)?.reference
            }
        } catch {
            tab.roles = .failed(error.localizedDescription)
        }
        await loadGrants(tab)
    }

    func loadGrants(_ tab: UsersTab) async {
        guard let role = tab.selectedRole else {
            tab.grants = .idle
            return
        }
        // Keep showing the previous role's rows only while reloading the same role.
        if tab.grants.value == nil { tab.grants = .loading }
        do {
            let grants = try await driver(for: tab.connection).listGrants(of: role)
            guard tab.selectedRole == role else { return }
            tab.grants = .loaded(grants)
        } catch {
            guard tab.selectedRole == role else { return }
            tab.grants = .failed(error.localizedDescription)
        }
    }

    func selectRole(_ role: RoleRef?, in tab: UsersTab) {
        guard role != tab.selectedRole else { return }
        tab.selectedRole = role
        tab.grants = role == nil ? .idle : .loading
        Task { await loadGrants(tab) }
    }

    /// The SQL a change would run (passwords masked in `display`).
    func previewAccess(_ change: AccessChange, in tab: UsersTab) throws -> [AccessStatement] {
        try driver(for: tab.connection).previewAccess(change)
    }

    /// Runs a change, then reloads the list (selecting the changed role) and its privileges.
    func applyAccess(_ change: AccessChange, in tab: UsersTab) async throws {
        try await driver(for: tab.connection).applyAccess(change)
        let select: RoleRef? = switch change {
        case .createRole(let spec), .alterRole(_, let spec):
            RoleRef(name: spec.name.trimmingCharacters(in: .whitespaces), host: tab.features.hosts ? (spec.host?.isEmpty == false ? spec.host : "%") : nil)
        case .dropRole: nil
        case .setPrivileges(let role, _, _, _): role
        }
        if case .dropRole = change { tab.selectedRole = nil }
        await loadRoles(tab, select: select)
    }

    /// Drop / revoke confirmed in an alert: failures are shown in another alert.
    func confirmAccess(_ change: AccessChange, in tab: UsersTab) {
        Task {
            do {
                try await applyAccess(change, in: tab)
            } catch {
                tab.actionError = error.localizedDescription
            }
        }
    }
}
