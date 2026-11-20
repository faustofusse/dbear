import DBKit
import SwiftUI

/// The connection editor's fields for Cloudflare D1: the account ID (in `host`), the database's name
/// or ID, and how to authenticate: "Sign in with Cloudflare" or an API token. Either one is the
/// connection's password (Keychain). A sign-in also puts the account's name in `user`, so the editor
/// knows a saved connection is signed in without reading the Keychain.
struct D1ConnectionSection<KindPicker: View>: View {
    @Binding var draft: ConnectionConfig
    /// The editor's password field: the API token, or the sign-in's secret.
    @Binding var token: String
    let tokenPrompt: String
    /// Called when the token is typed or pasted, so the editor saves it.
    let tokenEdited: () -> Void
    @ViewBuilder let kindPicker: KindPicker

    @Environment(\.openURL) private var openURL
    @State private var showsToken = false
    @State private var signInTask: Task<Void, Never>?
    @State private var signInError: String?
    /// What the sign-in reaches: the account picker and database menu.
    @State private var accounts: [CloudflareAccount] = []
    @State private var databases: [String] = []

    /// A sign-in made in this editor, or the saved connection's (until signed out).
    private var signedIn: Bool {
        CloudflareSignIn.isSignIn(token) || (token.isEmpty && draft.user != nil)
    }

    var body: some View {
        Section {
            kindPicker
            if accounts.count > 1 {
                Picker("Account", selection: accountBinding) {
                    if !accounts.contains(where: { $0.id == draft.host }) { Text("Choose…").tag("") }
                    ForEach(accounts) { Text($0.name).tag($0.id) }
                }
            } else {
                TextField("Account ID", text: pasteURL(into: \.host), prompt: Text("Optional"))
            }
            LabeledContent("Database") {
                HStack(spacing: 6) {
                    TextField("Database", text: pasteURL(into: \.database), prompt: Text("Name or ID"))
                        .labelsHidden()
                    if !databases.isEmpty {
                        Menu {
                            ForEach(databases, id: \.self) { name in
                                Button(name) { draft.database = name }
                            }
                        } label: {
                            Image(systemName: "chevron.up.chevron.down")
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .fixedSize()
                        .help("Choose one of the account's databases")
                    }
                }
            }
            Toggle(isOn: $draft.showAllDatabases) {
                Text("Show all databases")
                Text("Switch between the account's D1 databases from the tables column's title. The one above opens first.")
            }
        } header: {
            Text("Database")
        } footer: {
            Text("The database is its `database_name` in wrangler.toml, or its ID. With an API token, the account ID is under Workers & Pages ▸ Account details in the Cloudflare dashboard; leave it empty if the token can only see one account.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        Section {
            LabeledContent("Cloudflare") {
                if signInTask != nil {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Waiting for your browser…").foregroundStyle(.secondary)
                        Button("Cancel") { cancelSignIn() }
                    }
                } else if signedIn {
                    HStack(spacing: 8) {
                        Label(draft.user.map { "Signed in to \($0)" } ?? "Signed in", systemImage: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                        Button("Sign Out") { signOut() }
                    }
                } else {
                    Button("Sign in with Cloudflare") { signIn() }
                        .buttonStyle(.borderedProminent)
                }
            }
            if !signedIn {
                LabeledContent("API Token") {
                    HStack(spacing: 6) {
                        Group {
                            if showsToken {
                                TextField("API Token", text: $token, prompt: Text(tokenPrompt))
                            } else {
                                SecureField("API Token", text: $token, prompt: Text(tokenPrompt))
                            }
                        }
                        .labelsHidden()
                        .onChange(of: token) { tokenEdited() }
                        Button {
                            showsToken.toggle()
                        } label: {
                            Image(systemName: showsToken ? "eye.slash" : "eye")
                        }
                        .buttonStyle(.borderless)
                        .help(showsToken ? "Hide the token" : "Show the token")
                    }
                }
            }
        } header: {
            Text("Authentication")
        } footer: {
            Group {
                if let signInError {
                    Text(signInError).foregroundStyle(.red)
                } else if signedIn {
                    Text("The sign-in is kept in your login Keychain and renewed as needed.")
                        .foregroundStyle(.secondary)
                } else {
                    Text("Sign in to pick the account and database, or create an API token in the Cloudflare dashboard under My Profile ▸ API Tokens, with D1 Read to browse or D1 Edit to change data. Either is stored in your login Keychain, never in the connections file.")
                        .foregroundStyle(.secondary)
                }
            }
            .font(.caption)
        }
        .onDisappear { cancelSignIn() }
    }

    private var accountBinding: Binding<String> {
        Binding(
            get: { draft.host },
            set: { id in
                guard let account = accounts.first(where: { $0.id == id }) else { return }
                choose(account)
            }
        )
    }

    private func signIn() {
        signInError = nil
        signInTask = Task { @MainActor in
            defer { signInTask = nil }
            do {
                let secret = try await CloudflareSignIn.signIn { openURL($0) }
                token = secret
                tokenEdited()
                draft.user = "Cloudflare"
                await loadAccounts(secret: secret)
            } catch is CancellationError {
            } catch {
                signInError = error.localizedDescription
            }
        }
    }

    private func cancelSignIn() {
        signInTask?.cancel()
        signInTask = nil
    }

    private func signOut() {
        if CloudflareSignIn.isSignIn(token) {
            let secret = token
            Task { await CloudflareSignIn.revoke(secret) }
        }
        token = ""
        tokenEdited()
        draft.user = nil
        accounts = []
        databases = []
    }

    private func loadAccounts(secret: String) async {
        do {
            accounts = try await CloudflareSignIn.accounts(secret: secret)
            let host = draft.host.trimmingCharacters(in: .whitespaces)
            if host.isEmpty, accounts.count == 1 {
                choose(accounts[0])
            } else if let account = accounts.first(where: { $0.id == host }) {
                choose(account)
            }
        } catch {
            signInError = error.localizedDescription
        }
    }

    private func choose(_ account: CloudflareAccount) {
        draft.host = account.id
        draft.user = account.name
        databases = []
        guard CloudflareSignIn.isSignIn(token) else { return }
        let secret = token
        Task { @MainActor in
            do {
                let names = try await CloudflareSignIn.databases(secret: secret, account: account.id)
                if draft.host == account.id { databases = names }
            } catch {
                signInError = error.localizedDescription
            }
        }
    }

    /// Pasting a whole `d1://<account>/<database>?token=…` URL into either field fills in every field.
    private func pasteURL(into field: WritableKeyPath<ConnectionConfig, String>) -> Binding<String> {
        Binding(
            get: { draft[keyPath: field] },
            set: { value in
                let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
                guard trimmed.hasPrefix("d1://"), let parsed = try? ConnectionConfig.parse(url: trimmed), parsed.kind == .d1
                else {
                    draft[keyPath: field] = value
                    return
                }
                draft.host = parsed.host
                draft.database = parsed.database
                if let parsedToken = parsed.password {
                    token = parsedToken
                    tokenEdited()
                    draft.user = nil
                }
            }
        )
    }
}
