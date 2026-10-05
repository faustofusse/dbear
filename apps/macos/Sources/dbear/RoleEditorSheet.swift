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

    private var change: AccessChange {
        if let original { .alterRole(original, spec) } else { .createRole(spec) }
    }

    /// The statements to run, or why there aren't any.
    private var preview: Result<[AccessStatement], Error> {
        Result { try model.previewAccess(change, in: tab) }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Form {
                identitySection
                permissionsSection
                if features.connectionLimit || features.validUntil { limitsSection }
                if features.membership { membershipSection }
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
            SecureField("Password", text: $spec.password, prompt: Text(isNew ? "None" : "Unchanged"))
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
            switch preview {
            case .success(let statements) where statements.isEmpty:
                Text("Nothing changed.").foregroundStyle(.secondary)
            case .success(let statements):
                DDLView(sql: statements.map(\.display).joined(separator: ";\n") + ";", fontSize: 11)
                    .listRowInsets(EdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6))
            case .failure(let error):
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
        if case .success(let statements) = preview { !statements.isEmpty } else { false }
    }

    private func save() {
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
