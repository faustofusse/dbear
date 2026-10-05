import DBKit
import SwiftUI

/// "New Role" / "Edit Role": name, password, attributes and memberships, with the SQL it will run.
struct RoleEditorSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let tab: UsersTab
    let original: Role?

    @State private var spec: RoleSpec
    @State private var limitText: String
    @State private var showsBuiltInRoles = false
    @State private var showsPassword = false
    /// Privileges on each database as listed, and the ones picked here (by database name).
    @State private var access: LoadState<[DatabaseAccess]> = .loading
    /// The level picked per database, and what each database's current level is (read there).
    @State private var wantedLevels: [String: DatabaseLevel] = [:]
    @State private var contexts: [String: DatabaseLevelContext] = [:]
    @State private var probeErrors: [String: String] = [:]
    @State private var saving = false
    @State private var error: String?

    init(tab: UsersTab, original: Role?) {
        self.tab = tab
        self.original = original
        var spec = original.map(RoleSpec.init) ?? RoleSpec()
        if original == nil, tab.features.hosts { spec.host = "%" }
        _spec = State(initialValue: spec)
        _limitText = State(initialValue: original?.connectionLimit.map(String.init) ?? "")
    }

    private var isNew: Bool { original == nil }
    private var features: AccessFeatures { tab.features }
    private var noun: String { tab.connection.kind == .mysql ? "User" : "Role" }

    private var levels: [DatabaseLevel] { Access.levels(tab.connection.kind) }

    /// Databases whose new level is picked but whose current one is still being read.
    private var pendingLevels: [String] {
        wantedLevels.keys.filter { contexts[$0] == nil }.sorted()
    }

    /// The role, then database levels: granted to the role's new name.
    private var changes: [AccessChange] {
        var changes: [AccessChange] = [original.map { .alterRole($0, spec) } ?? .createRole(spec)]
        let role = tab.reference(for: spec)
        for (database, level) in wantedLevels.sorted(by: { $0.key < $1.key }) {
            guard let context = contexts[database], context.level != level else { continue }
            changes.append(.setDatabaseLevel(role: role, context: context, level: level))
        }
        return changes
    }

    /// The statements to run, or why there aren't any.
    private var preview: Result<[AccessStatement], Error> {
        Result { try model.previewAccess(changes, in: tab) }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Form {
                identitySection
                permissionsSection
                if features.connectionLimit || features.validUntil { limitsSection }
                if features.membership { membershipSection }
                accessSection
                sqlSection
            }
            .formStyle(.grouped)
            .scrollBounceBehavior(.basedOnSize)
            footer
        }
        .frame(width: 540, height: 640)
        .onChange(of: limitText) {
            let digits = limitText.filter(\.isNumber)
            if digits != limitText { limitText = digits }
            spec.connectionLimit = Int(digits)
        }
        .onChange(of: spec) { error = nil }
        .onChange(of: wantedLevels) { error = nil }
        .task { await loadAccess() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Image(systemName: spec.canLogin ? "person.fill" : "person.2.fill")
                .font(.system(size: 18))
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(.tint, in: Circle())
            VStack(alignment: .leading, spacing: 2) {
                Text(isNew ? "New \(noun)" : "Edit \(original?.reference.title ?? noun)").font(.headline)
                Text("\(tab.connection.kind.displayName) · \(tab.connection.host)")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer()
        }
        .padding(.horizontal, 20)
        .padding(.top, 20)
    }

    // MARK: Sections

    private var identitySection: some View {
        Section {
            TextField("Name", text: $spec.name, prompt: Text(verbatim: tab.connection.kind == .mysql ? "app_user" : "app_reader"))
            if features.hosts {
                TextField("Host", text: Binding(get: { spec.host ?? "" }, set: { spec.host = $0 }), prompt: Text(verbatim: "%"))
            }
            passwordField
        } footer: {
            if features.hosts {
                Text("Host is where the user may connect from: % for anywhere, localhost, or an address pattern like 10.0.%.")
                    .font(.caption).foregroundStyle(.secondary)
            } else if !isNew, original?.name != spec.name.trimmingCharacters(in: .whitespaces) {
                Text("Renaming a role clears its MD5 password; set a new one.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    /// Hidden or shown, with Generate.
    private var passwordField: some View {
        LabeledContent("Password") {
            HStack(spacing: 6) {
                Group {
                    if showsPassword {
                        TextField("Password", text: $spec.password, prompt: Text(isNew ? "None" : "Unchanged"))
                            .font(.system(.body, design: .monospaced))
                    } else {
                        SecureField("Password", text: $spec.password, prompt: Text(isNew ? "None" : "Unchanged"))
                    }
                }
                .labelsHidden()
                .multilineTextAlignment(.trailing)
                Button {
                    showsPassword.toggle()
                } label: {
                    Image(systemName: showsPassword ? "eye.slash" : "eye")
                }
                .help(showsPassword ? "Hide password" : "Show password")
                .disabled(spec.password.isEmpty)
                Button("Generate") { generatePassword() }
                    .help("Fill in a strong random password")
            }
            .buttonStyle(.borderless)
        }
    }

    private func generatePassword() {
        do {
            spec.password = try Access.generatePassword()
            showsPassword = true
        } catch {
            self.error = error.localizedDescription
        }
    }

    // MARK: Database access

    @ViewBuilder
    private var accessSection: some View {
        Section {
            switch access {
            case .idle, .loading:
                ProgressView().controlSize(.small).frame(maxWidth: .infinity)
            case .failed(let message):
                Text(message).foregroundStyle(.secondary)
            case .loaded(let databases):
                ForEach(databases) { database in accessRow(database) }
            }
        } header: {
            Text("Database Access")
        } footer: {
            Text(accessFooter).font(.caption).foregroundStyle(.secondary)
        }
    }

    private var accessFooter: String {
        if tab.connection.kind == .postgres {
            var text = "Levels cover every schema in the database, and tables, sequences and schemas created later. Picking one replaces what the role had there."
            if spec.isSuperuser { text = "Superusers can do anything in every database. " + text }
            return text
        }
        return "Privileges on every table of the database (database.*), including ones created later."
    }

    private func accessRow(_ database: DatabaseAccess) -> some View {
        let name = database.database
        let current = contexts[name]?.level
        let shown = wantedLevels[name] ?? current ?? database.level
        return LabeledContent {
            if database.isOwner {
                Text("Owner").foregroundStyle(.secondary)
            } else if current == nil && database.level == .custom && wantedLevels[name] == nil {
                // Has privileges there: its level is being read in that database.
                if probeErrors[name] != nil {
                    Image(systemName: "exclamationmark.triangle").foregroundStyle(.secondary).help(probeErrors[name] ?? "")
                } else {
                    ProgressView().controlSize(.small)
                }
            } else {
                Picker(name, selection: Binding(get: { shown }, set: { pick($0, for: database) })) {
                    ForEach(levels, id: \.self) { level in Text(level.title).tag(level) }
                    if shown == .custom || current == .custom {
                        Divider()
                        Text("Custom (unchanged)").tag(DatabaseLevel.custom)
                    }
                }
                .labelsHidden()
                .fixedSize()
            }
        } label: {
            Text(name)
            if let error = probeErrors[name], wantedLevels[name] != nil {
                Text(error).foregroundStyle(.red)
            } else if !database.isOwner, shown != .noAccess {
                Text(shown == .custom ? customDescription(database) : shown.summary(tab.connection.kind))
            } else if database.everyoneCanConnect {
                Text("Anyone can connect (granted to PUBLIC), but not read tables.")
            }
        }
    }

    private func customDescription(_ database: DatabaseAccess) -> String {
        let privileges = (contexts[database.database]?.privileges ?? database.privileges).privileges
        return privileges.isEmpty
            ? "Privileges inside the database that match no level."
            : "\(privileges.joined(separator: ", ")) on the database, and privileges inside it that match no level."
    }

    /// Picks a level; its current one must be known first (read in that database).
    private func pick(_ level: DatabaseLevel, for database: DatabaseAccess) {
        let name = database.database
        if level == .custom || level == contexts[name]?.level {
            wantedLevels[name] = nil
            return
        }
        wantedLevels[name] = level
        if contexts[name] == nil { Task { await probe(name) } }
    }

    private func probe(_ database: String) async {
        probeErrors[database] = nil
        do {
            contexts[database] = try await model.driver(for: tab.connection).databaseLevel(of: probeRole, in: database)
        } catch {
            probeErrors[database] = error.localizedDescription
        }
    }

    /// The role to read levels for: a new role has none, so a name nobody has.
    private var probeRole: RoleRef {
        original?.reference ?? newRoleProbe
    }

    @State private var newRoleProbe = RoleRef(name: "dbear-new-\(UUID().uuidString)")

    private func loadAccess() async {
        do {
            let databases = try await model.driver(for: tab.connection).listDatabaseAccess(of: probeRole)
            access = .loaded(databases)
            if tab.connection.kind == .mysql {
                // `db.*` privileges tell the level: nothing to read elsewhere.
                for database in databases {
                    contexts[database.database] = DatabaseLevelContext(database: database.database, level: database.level, privileges: database.privileges)
                }
            } else {
                await withTaskGroup(of: Void.self) { group in
                    for database in databases where database.level == .custom && !database.isOwner {
                        group.addTask { await probe(database.database) }
                    }
                }
            }
        } catch {
            access = .failed(error.localizedDescription)
        }
    }

    private var permissionsSection: some View {
        Section("Permissions") {
            Toggle(isOn: $spec.canLogin) {
                Text("Can log in")
                Text(features.hosts ? "Unchecked, the account is locked (a role for others to be granted)." : "Unchecked, it’s a group role others can be members of.")
            }
            if features.superuser {
                Toggle(isOn: $spec.isSuperuser) {
                    Text("Superuser")
                    Text("Bypasses every permission check.")
                }
            }
            if features.createDB { Toggle("Create databases", isOn: $spec.canCreateDB) }
            if features.createRole { Toggle("Create roles", isOn: $spec.canCreateRole) }
        }
    }

    private var limitsSection: some View {
        Section("Limits") {
            if features.connectionLimit {
                TextField("Connection limit", text: $limitText, prompt: Text("Unlimited"))
            }
            if features.validUntil {
                TextField(
                    "Password expires",
                    text: Binding(get: { spec.validUntil ?? "" }, set: { spec.validUntil = $0.isEmpty ? nil : $0 }),
                    prompt: Text("Never")
                )
                .help("A date or timestamp, like 2026-12-31")
            }
        }
    }

    @ViewBuilder
    private var membershipSection: some View {
        let candidates = tab.possibleParents(of: original?.reference)
        let regular = candidates.filter { !$0.isSystem }
        let builtIn = candidates.filter(\.isSystem)
        Section {
            if regular.isEmpty && builtIn.isEmpty {
                Text("There are no other roles.").foregroundStyle(.secondary)
            }
            ForEach(regular) { role in membershipToggle(role) }
            if !builtIn.isEmpty {
                DisclosureGroup("Built-in Roles", isExpanded: $showsBuiltInRoles) {
                    ForEach(builtIn) { role in membershipToggle(role) }
                }
            }
        } header: {
            Text("Member Of")
        } footer: {
            Text("Members inherit the privileges of the roles they belong to.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private func membershipToggle(_ role: Role) -> some View {
        Toggle(isOn: Binding(
            get: { spec.memberOf.contains(role.reference) },
            set: { on in
                if on { spec.memberOf.append(role.reference) } else { spec.memberOf.removeAll { $0 == role.reference } }
            }
        )) {
            Label(role.reference.title, systemImage: role.roleSymbol)
        }
    }

    @ViewBuilder
    private var sqlSection: some View {
        Section("SQL") {
            if !pendingLevels.isEmpty {
                Label("Reading access in \(pendingLevels.formatted(.list(type: .and)))\u{2026}", systemImage: "hourglass")
                    .foregroundStyle(.secondary)
            }
            switch preview {
            case .success(let statements) where statements.isEmpty:
                Text("Nothing changed.").foregroundStyle(.secondary)
            case .success:
                DDLView(sql: previewText, fontSize: 11)
                    .listRowInsets(EdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6))
            case .failure(let error):
                Text(error.localizedDescription).foregroundStyle(.secondary)
            }
        }
    }

    /// The statements, with a comment before those that run in another database (Postgres levels).
    private var previewText: String {
        var lines: [String] = []
        var database: String?
        for change in changes {
            guard let statements = try? model.previewAccess(change, in: tab), !statements.isEmpty else { continue }
            var runsIn: String?
            if case .setDatabaseLevel(_, let context, _) = change, tab.connection.kind == .postgres {
                runsIn = context.database
            }
            if runsIn != database {
                if !lines.isEmpty { lines.append("") }
                lines.append("-- in \(runsIn ?? tab.connection.defaultDatabase)")
                database = runsIn
            }
            lines += statements.map { $0.display + ";" }
        }
        return lines.joined(separator: "\n")
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
                Button(isNew ? "Create" : "Save") { save() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(saving || !canSave)
            }
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
        .padding(.top, 4)
    }

    private var canSave: Bool {
        guard pendingLevels.isEmpty, case .success(let statements) = preview else { return false }
        return !statements.isEmpty
    }

    private func save() {
        saving = true
        error = nil
        Task {
            do {
                try await model.applyAccess(changes, in: tab)
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            saving = false
        }
    }
}
