import DBKit
import Foundation
import Observation

/// Users & roles of a connection: listed in the middle column (users mode), the selected one shown
/// in the users tab. One per connection: roles are server-wide, in Postgres and MySQL alike.
@Observable
@MainActor
final class UsersTab: Identifiable {
    let id = UUID()
    /// The connection, pointed at the database whose privileges are shown (Postgres keeps them per
    /// database; it follows the database picked in the middle column's title). MySQL's are server-wide.
    var connection: ConnectionConfig
    let features: AccessFeatures

    var roles: LoadState<[Role]> = .idle
    var selectedRole: RoleRef?
    /// The selected role's privileges, grouped by object.
    var grants: LoadState<[ObjectPrivileges]> = .idle
    /// The selected role's privileges on each database of the server.
    var databaseAccess: LoadState<[DatabaseAccess]> = .idle
    /// Its level in databases where it has privileges (Postgres reads them in each database).
    var databaseLevels: [String: DatabaseLevel] = [:]
    /// Built-in roles (`pg_*`, `mysql.sys`…) are listed too.
    var showsSystemRoles = false
    var search = ""

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

    /// The role a spec names: trimmed, MySQL hosts defaulting to `%`.
    func reference(for spec: RoleSpec) -> RoleRef {
        let host = spec.host?.trimmingCharacters(in: .whitespaces)
        return RoleRef(name: spec.name.trimmingCharacters(in: .whitespaces), host: features.hosts ? (host?.isEmpty == false ? host : "%") : nil)
    }

    /// Privileges a role holds on `object`, as listed.
    func privileges(on object: GrantObject) -> PrivilegeSet {
        grants.value?.first { $0.object == object }?.privileges ?? PrivilegeSet()
    }
}

/// What the middle column lists.
enum BrowseMode: Hashable {
    case tables, users
}

struct RoleEditorRequest: Identifiable {
    let id = UUID()
    let state: UsersTab
    /// `nil`: a new role.
    let original: Role?
}

struct PrivilegeEditorRequest: Identifiable {
    let id = UUID()
    let state: UsersTab
    let role: RoleRef
    /// The object to edit; `nil` to pick one (new grant).
    let object: GrantObject?
}

struct PendingRoleDrop {
    let state: UsersTab
    let role: Role
}

struct PendingRevoke {
    let state: UsersTab
    let role: RoleRef
    let grant: ObjectPrivileges
}

extension AppModel {
    var activeUsersTab: UsersTab? {
        if case .users(let t) = activeTab { t } else { nil }
    }

    /// Whether `connection`'s users can be managed here.
    func canManageUsers(_ connection: ConnectionConfig) -> Bool {
        Access.features(connection.kind) != nil
    }

    /// The middle column lists users (switch in its toolbar) and the connection supports it.
    var showsUsers: Bool {
        browseMode == .users && (selectedConnection.map(canManageUsers) ?? false)
    }

    /// The selected connection's users list, once `prepareUsers` made it.
    var currentUsers: UsersTab? {
        selectedConnectionID.flatMap { usersStates[$0] }
    }

    /// Makes (and loads) the selected connection's users list for the middle column, pointed at the
    /// selected database. Switching database only reloads the privileges (Postgres), not the roles.
    @discardableResult
    func prepareUsers() async -> UsersTab? {
        guard let (state, retargeted) = usersState() else { return nil }
        if state.roles.value == nil && !state.roles.isLoading {
            await loadRoles(state)
        } else if retargeted {
            await loadGrants(state)
        }
        return state
    }

    /// Lists users in the middle column and shows the selected one (Users & Roles menu, ⇧⌘U).
    func openUsers() {
        guard let (state, retargeted) = usersState() else { return }
        browseMode = .users
        show(state)
        if state.roles.value == nil {
            Task { await loadRoles(state) }
        } else if retargeted {
            Task { await loadGrants(state) }
        }
    }

    /// The selected connection's users state, made if needed and pointed at the selected database.
    /// `retargeted`: it was showing another database's privileges, which need reloading.
    private func usersState() -> (UsersTab, retargeted: Bool)? {
        guard let target = selectedTarget, let features = Access.features(target.kind) else { return nil }
        guard let state = usersStates[target.id] else {
            let state = UsersTab(connection: target, features: features)
            usersStates[target.id] = state
            return (state, false)
        }
        guard features.grantsPerDatabase, state.connection.driverKey != target.driverKey else { return (state, false) }
        state.connection = target
        // The other database's privileges are wrong here, not just stale.
        state.grants = state.selectedRole == nil ? .idle : .loading
        return (state, true)
    }

    /// Selects a role in the middle column and shows it in the users tab.
    func showRole(_ role: RoleRef, in state: UsersTab) {
        selectRole(role, in: state)
        show(state)
    }

    /// Opens (or activates) the tab showing `state`'s selected role.
    private func show(_ state: UsersTab) {
        if !tabs.contains(where: { $0.id == state.id }) {
            tabs.insert(.users(state), at: (tabs.firstIndex { $0.id == activeTabID }).map { $0 + 1 } ?? tabs.count)
        }
        activeTabID = state.id
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
            } else if !tabs.contains(where: { $0.id == tab.id }) {
                // Just listed in the middle column: nothing is shown yet, so nothing is selected.
                tab.selectedRole = nil
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
            tab.databaseAccess = .idle
            return
        }
        async let access: Void = loadDatabaseAccess(tab, role: role)
        // Keep showing the previous role's rows only while reloading the same role.
        if tab.grants.value == nil { tab.grants = .loading }
        let target = tab.connection.driverKey
        do {
            let grants = try await driver(for: tab.connection).listGrants(of: role)
            guard tab.selectedRole == role, tab.connection.driverKey == target else { return }
            tab.grants = .loaded(grants)
        } catch {
            guard tab.selectedRole == role, tab.connection.driverKey == target else { return }
            tab.grants = .failed(error.localizedDescription)
        }
        await access
    }

    private func loadDatabaseAccess(_ tab: UsersTab, role: RoleRef) async {
        if tab.databaseAccess.value == nil { tab.databaseAccess = .loading }
        do {
            let access = try await driver(for: tab.connection).listDatabaseAccess(of: role)
            guard tab.selectedRole == role else { return }
            tab.databaseLevels = [:]
            tab.databaseAccess = .loaded(access)
            // Postgres: what the role has inside each database it has privileges on.
            let probed = access.filter { $0.level == .custom && !$0.isOwner }.map(\.database)
            let driver = driver(for: tab.connection)
            await withTaskGroup(of: (String, DatabaseLevel?).self) { group in
                for database in probed {
                    group.addTask { (database, try? await driver.databaseLevel(of: role, in: database).level) }
                }
                for await (database, level) in group where tab.selectedRole == role {
                    if let level { tab.databaseLevels[database] = level }
                }
            }
        } catch {
            guard tab.selectedRole == role else { return }
            tab.databaseAccess = .failed(error.localizedDescription)
        }
    }

    func selectRole(_ role: RoleRef?, in tab: UsersTab) {
        guard role != tab.selectedRole else { return }
        tab.selectedRole = role
        tab.grants = role == nil ? .idle : .loading
        tab.databaseAccess = role == nil ? .idle : .loading
        Task { await loadGrants(tab) }
    }

    /// The SQL a change would run (passwords masked in `display`).
    func previewAccess(_ change: AccessChange, in tab: UsersTab) throws -> [AccessStatement] {
        try previewAccess([change], in: tab)
    }

    func previewAccess(_ changes: [AccessChange], in tab: UsersTab) throws -> [AccessStatement] {
        try driver(for: tab.connection).previewAccess(changes)
    }

    func applyAccess(_ change: AccessChange, in tab: UsersTab) async throws {
        try await applyAccess([change], in: tab)
    }

    /// Runs changes (in one transaction where possible), then reloads the list (selecting the changed role) and its privileges.
    func applyAccess(_ changes: [AccessChange], in tab: UsersTab) async throws {
        try await driver(for: tab.connection).applyAccess(changes)
        var select: RoleRef?
        for change in changes {
            switch change {
            case .createRole(let spec), .alterRole(_, let spec):
                select = tab.reference(for: spec)
            case .dropRole:
                tab.selectedRole = nil
            case .setPrivileges(let role, _, _, _), .setDatabaseLevel(let role, _, _):
                select = select ?? role
            }
        }
        await loadRoles(tab, select: select)
    }

    /// Drop / revoke confirmed in an alert: failures are shown in another alert.
    func confirmAccess(_ change: AccessChange, in tab: UsersTab) {
        Task {
            do {
                try await applyAccess(change, in: tab)
            } catch {
                self.accessError = error.localizedDescription
            }
        }
    }
}

