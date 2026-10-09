import DBKit
import SwiftUI
import UniformTypeIdentifiers

/// "New Connection" / "Edit Connection" sheet.
struct ConnectionEditor: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    let original: ConnectionConfig?

    @State private var draft: ConnectionConfig
    @State private var password = ""
    /// The password field was touched; otherwise the Keychain copy is kept as is.
    @State private var passwordEdited = false
    @State private var hasSavedPassword = false
    /// The SSH password or key passphrase, handled like `password` (Keychain, untouched = kept).
    @State private var sshSecret = ""
    @State private var sshSecretEdited = false
    @State private var hasSavedSSHSecret = false
    @State private var url = ""
    @State private var urlError: String?
    @State private var test: TestState = .idle
    @State private var saveError: String?

    enum TestState: Equatable {
        case idle, running, succeeded
        case failed(String)
    }

    init(original: ConnectionConfig?) {
        self.original = original
        _draft = State(initialValue: original ?? .blank(.postgres))
    }

    private var isNew: Bool { original == nil }
    private var validationError: String? { draft.validationError }

    var body: some View {
        VStack(spacing: 0) {
            header
            Form {
                if isNew { urlSection }
                generalSection
                if draft.kind == .sqlite {
                    fileSection
                } else if draft.kind == .libsql {
                    TursoConnectionSection(
                        draft: $draft, token: $password, tokenPrompt: passwordPrompt,
                        tokenEdited: { passwordEdited = true }
                    ) { kindPicker }
                } else {
                    serverSection
                    authSection
                    sshSection
                }
            }
            .formStyle(.grouped)
            .scrollBounceBehavior(.basedOnSize)
            footer
        }
        .frame(width: 520)
        .fixedSize(horizontal: false, vertical: true)
        .onAppear {
            hasSavedPassword = model.hasSavedPassword(draft.id)
            hasSavedSSHSecret = model.hasSavedSSHSecret(draft.id)
        }
        .onChange(of: draft) { test = .idle; saveError = nil }
        .onChange(of: draft.kind) { old, new in kindChanged(from: old, to: new) }
        .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.item]) { result in
            if case .success(let url) = result { draft.database = url.path }
        }
        .onChange(of: password) { test = .idle }
        .onChange(of: sshSecret) { test = .idle }
    }

    // MARK: Sections

    private var header: some View {
        HStack(spacing: 12) {
            DatabaseKindIcon(kind: draft.kind, size: 24)
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(.tint, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            VStack(alignment: .leading, spacing: 2) {
                Text(isNew ? "New Connection" : "Edit Connection")
                    .font(.headline)
                Text(draft.validationError == nil ? draft.refreshed.summary : draft.kind.displayName)
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

    private var urlSection: some View {
        Section {
            TextField("URL", text: $url, prompt: Text(verbatim: "postgres://user:password@host:5432/database"))
                .textContentType(.URL)
                .onChange(of: url) { apply(url: url) }
        } footer: {
            Text(urlError ?? "Paste a connection URL to fill in the fields below.")
                .foregroundStyle(urlError == nil ? AnyShapeStyle(.secondary) : AnyShapeStyle(.red))
                .font(.caption)
        }
    }

    private var generalSection: some View {
        Section {
            // Left empty, the name is the database (or host); the placeholder shows which.
            TextField("Name", text: $draft.name, prompt: Text(verbatim: draft.defaultName.isEmpty ? "Optional" : draft.defaultName))
            LabeledContent("Group") {
                HStack(spacing: 4) {
                    TextField("Group", text: $draft.group, prompt: Text("None"))
                        .labelsHidden()
                    if !existingGroups.isEmpty {
                        Menu {
                            ForEach(existingGroups, id: \.self) { group in
                                Button(group) { draft.group = group }
                            }
                        } label: {
                            Image(systemName: "chevron.up.chevron.down")
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                        .help("Choose an existing group")
                    }
                }
            }
        }
    }

    private var kindPicker: some View {
        Picker("Type", selection: $draft.kind) {
            ForEach(DatabaseKind.allCases, id: \.self) { kind in
                Text(kind.displayName).tag(kind)
            }
        }
    }

    private var fileSection: some View {
        Section("Database") {
            kindPicker
            LabeledContent("File") {
                HStack(spacing: 6) {
                    TextField("File", text: $draft.database, prompt: Text("Required"))
                        .labelsHidden()
                        .truncationMode(.head)
                    Button("Choose…") { choosingFile = true }
                }
            }
        }
    }

    private var serverSection: some View {
        Section("Server") {
            kindPicker
            TextField("Host", text: $draft.host, prompt: Text(verbatim: "localhost"))
            TextField(
                "Port", value: $draft.port, format: .number.grouping(.never),
                prompt: Text(verbatim: draft.kind.defaultPort.map(String.init) ?? "")
            )
            TextField("Database", text: $draft.database, prompt: Text(verbatim: databasePrompt))
            Toggle(isOn: $draft.showAllDatabases) {
                Text("Show all databases")
                Text(draft.database.trimmingCharacters(in: .whitespaces).isEmpty && draft.kind == .mysql
                     ? "Switch between the server's databases from the tables column's title. The first one opens."
                     : "Switch between the server's databases from the tables column's title. The one above opens first.")
            }
        }
    }

    /// SQLite needs its file; servers fall back to their default database.
    private var databasePrompt: String {
        if draft.kind == .sqlite { return "Required" }
        let fallback = ConnectionConfig(id: "", name: "", group: "", kind: draft.kind, host: "", database: "").defaultDatabase
        if !fallback.isEmpty { return "Optional (\(fallback))" }
        return draft.showAllDatabases ? "Optional (all databases)" : "Optional"
    }

    /// The user each engine logs in as by default.
    private var userPrompt: String {
        switch draft.kind {
        case .mysql: "root"
        case .sqlServer: "sa"
        default: "postgres"
        }
    }

    private var authSection: some View {
        Section {
            TextField("User", text: userBinding, prompt: Text(verbatim: userPrompt))
            SecureField("Password", text: $password, prompt: Text(passwordPrompt))
                .onChange(of: password) { passwordEdited = true }
            Picker("SSL", selection: $draft.sslMode) {
                // SQL Server always encrypts the login; "disable" leaves only the rest in the clear.
                Text(draft.kind == .sqlServer ? "Login Only" : "Disable").tag(SslMode.disable)
                Text("Prefer").tag(SslMode.prefer)
                Text("Require").tag(SslMode.require)
                Text("Verify Certificate").tag(SslMode.verifyFull)
            }
        } header: {
            Text("Authentication")
        } footer: {
            Text("Passwords are stored in your login Keychain, never in the connections file.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    /// Reach the server through an SSH server (`ssh -L`): its address, user and how to sign in.
    private var sshSection: some View {
        Section {
            Toggle("Connect through SSH", isOn: usesSSH)
            if draft.ssh != nil {
                TextField("SSH Host", text: sshField(\.host), prompt: Text(verbatim: "bastion.example.com"))
                TextField("SSH Port", value: sshPort, format: .number.grouping(.never), prompt: Text(verbatim: "22"))
                TextField("SSH User", text: sshField(\.user), prompt: Text("Required"))
                Picker("Sign In With", selection: sshField(\.auth)) {
                    Text("Password").tag(SshAuth.password)
                    Text("Private Key").tag(SshAuth.privateKey)
                    Text("SSH Agent").tag(SshAuth.agent)
                }
                switch draft.ssh?.auth {
                case .password:
                    SecureField("SSH Password", text: $sshSecret, prompt: Text(sshSecretPrompt))
                        .onChange(of: sshSecret) { sshSecretEdited = true }
                case .privateKey:
                    LabeledContent("Key File") {
                        HStack(spacing: 6) {
                            TextField("Key File", text: sshField(\.keyPath), prompt: Text(verbatim: "~/.ssh/id_ed25519"))
                                .labelsHidden()
                                .truncationMode(.head)
                            Button("Choose…") { chooseKeyFile() }
                        }
                    }
                    SecureField("Passphrase", text: $sshSecret, prompt: Text(sshSecretPrompt))
                        .onChange(of: sshSecret) { sshSecretEdited = true }
                default:
                    EmptyView()
                }
            }
        } header: {
            Text("SSH Tunnel")
        } footer: {
            if draft.ssh != nil {
                Text("Host and port above are as seen from the SSH server. The first time a server is used its host key is remembered, and a different key later is refused.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var usesSSH: Binding<Bool> {
        Binding(get: { draft.ssh != nil }, set: { on in
            draft.ssh = on ? (original?.ssh ?? SshTunnel(user: NSUserName())) : nil
        })
    }

    private func sshField<T>(_ field: WritableKeyPath<SshTunnel, T>) -> Binding<T> {
        Binding(
            get: { (draft.ssh ?? SshTunnel())[keyPath: field] },
            set: { draft.ssh?[keyPath: field] = $0 }
        )
    }

    private var sshPort: Binding<Int?> {
        Binding(get: { draft.ssh?.port }, set: { draft.ssh?.port = $0 })
    }

    private var sshSecretPrompt: String {
        if hasSavedSSHSecret && !sshSecretEdited { return "Saved in Keychain" }
        return draft.ssh?.auth == .privateKey ? "None" : "Required"
    }

    /// The SSH secret to connect with: what was typed, or the saved one if untouched.
    private var effectiveSSHSecret: String? {
        if sshSecretEdited { return sshSecret.isEmpty ? nil : sshSecret }
        return hasSavedSSHSecret ? model.savedSSHSecret(draft.id) : nil
    }

    /// An open panel that starts in ~/.ssh and shows hidden files (keys live in a hidden folder).
    private func chooseKeyFile() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh")
        panel.message = "Choose the private key to sign in to the SSH server with."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        draft.ssh?.keyPath = url.path.hasPrefix(home + "/") ? "~" + url.path.dropFirst(home.count) : url.path
    }

    private var footer: some View {
        HStack(spacing: 8) {
            Button("Test Connection") { Task { await runTest() } }
                .disabled(validationError != nil || test == .running)
            testStatus
            Spacer(minLength: 12)
            Button("Cancel", role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button(isNew ? "Add" : "Save") { save() }
                .keyboardShortcut(.defaultAction)
                .disabled(validationError != nil)
                .help(validationError ?? "")
        }
        .controlSize(.large)
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
    }

    @ViewBuilder
    private var testStatus: some View {
        let error = saveError.map(TestState.failed) ?? test
        switch error {
        case .idle:
            EmptyView()
        case .running:
            ProgressView().controlSize(.small)
            Text("Connecting…").foregroundStyle(.secondary)
        case .succeeded:
            Label("Connected", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        case .failed(let message):
            Label(message, systemImage: "xmark.octagon.fill")
                .foregroundStyle(.red)
                .lineLimit(2)
                .help(message)
                .textSelection(.enabled)
        }
    }

    // MARK: Helpers

    private var existingGroups: [String] {
        var seen = Set<String>()
        return model.connections.map(\.group).filter { !$0.isEmpty && seen.insert($0).inserted }
    }

    private var passwordPrompt: String {
        hasSavedPassword && !passwordEdited ? "Saved in Keychain" : "None"
    }

    private var userBinding: Binding<String> {
        Binding(get: { draft.user ?? "" }, set: { draft.user = $0.isEmpty ? nil : $0 })
    }

    /// The password to connect with: what was typed, or the saved one if untouched.
    private var effectivePassword: String? {
        if passwordEdited { return password.isEmpty ? nil : password }
        return hasSavedPassword ? model.savedPassword(draft.id) : nil
    }

    private func apply(url: String) {
        let trimmed = url.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { urlError = nil; return }
        do {
            let parsed = try ConnectionConfig.parse(url: trimmed)
            urlError = nil
            draft.kind = parsed.kind
            draft.host = parsed.host
            draft.port = parsed.port
            draft.database = parsed.database
            draft.user = parsed.user
            draft.sslMode = parsed.sslMode
            if draft.name.isEmpty || draft.name == lastAutoName { draft.name = parsed.name }
            lastAutoName = parsed.name
            if let pw = parsed.password { password = pw }
        } catch {
            urlError = error.localizedDescription
        }
    }
    @State private var lastAutoName = ""
    @State private var choosingFile = false

    /// Fields that mean something else for the new type are reset (a file path isn't a database name).
    private func kindChanged(from old: DatabaseKind, to new: DatabaseKind) {
        guard old != new else { return }
        if (old == .sqlite) != (new == .sqlite) { draft.database = "" }
        if new != .sqlite, draft.host.trimmingCharacters(in: .whitespaces).isEmpty { draft.host = "localhost" }
        if draft.port == old.defaultPort { draft.port = nil }
        if !draft.supportsSSH { draft.ssh = nil }
        // Turso: a remote host (not localhost), no user or database, certificate verified.
        if new == .libsql {
            if draft.host == "localhost" { draft.host = "" }
            draft.database = ""
            draft.user = nil
            draft.sslMode = .verifyFull
        } else if old == .libsql {
            draft.sslMode = .prefer
        }
    }

    private func runTest() async {
        test = .running
        var config = draft
        config.password = effectivePassword
        if config.ssh?.auth != .agent { config.ssh?.secret = effectiveSSHSecret }
        let error = await model.test(config)
        test = error.map(TestState.failed) ?? .succeeded
    }

    private func save() {
        do {
            let saved = try model.save(
                draft, password: passwordEdited ? password : nil, sshSecret: sshSecretEdited ? sshSecret : nil)
            if isNew { model.selectedConnectionID = saved.id }
            dismiss()
        } catch {
            saveError = error.localizedDescription
        }
    }
}
