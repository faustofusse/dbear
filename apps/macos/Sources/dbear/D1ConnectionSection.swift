import DBKit
import SwiftUI

/// The connection editor's fields for Cloudflare D1: the account ID (in `host`), the database's name
/// or ID, and an API token instead of a user and password (stored in the Keychain like one).
struct D1ConnectionSection<KindPicker: View>: View {
    @Binding var draft: ConnectionConfig
    /// The editor's password field: for D1 it holds the API token.
    @Binding var token: String
    let tokenPrompt: String
    /// Called when the token is typed or pasted, so the editor saves it.
    let tokenEdited: () -> Void
    @ViewBuilder let kindPicker: KindPicker

    @State private var showsToken = false

    var body: some View {
        Section {
            kindPicker
            TextField("Account ID", text: pasteURL(into: \.host), prompt: Text("Optional"))
            TextField("Database", text: pasteURL(into: \.database), prompt: Text("Name or ID"))
            Toggle(isOn: $draft.showAllDatabases) {
                Text("Show all databases")
                Text("Switch between the account's D1 databases from the tables column's title. The one above opens first.")
            }
        } header: {
            Text("Database")
        } footer: {
            Text("The account ID is under Workers & Pages ▸ Account details in the Cloudflare dashboard. Leave it empty if the token can only see one account. The database is its `database_name` in wrangler.toml, or its ID.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        Section {
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
        } header: {
            Text("Authentication")
        } footer: {
            Text("Create one in the Cloudflare dashboard under My Profile ▸ API Tokens, with the D1 Read permission to browse or D1 Edit to change data. Tokens are stored in your login Keychain, never in the connections file.")
                .font(.caption)
                .foregroundStyle(.secondary)
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
                if let parsedToken = parsed.password { token = parsedToken }
            }
        )
    }
}
