import DBKit
import SwiftUI
import UniformTypeIdentifiers

/// File ▸ Import from DBeaver…: lists DBeaver's saved connections to pick from.
struct ImportConnectionsSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    @State private var state: LoadState<ImportScan> = .loading
    @State private var selected: Set<ImportedConnection.ID> = []
    @State private var choosingFile = false
    @State private var source: String?
    @State private var importError: String?

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            content
                .frame(height: 380)
            Divider()
            footer
        }
        .frame(width: 560)
        .task { load(path: nil) }
        .fileImporter(isPresented: $choosingFile, allowedContentTypes: [.json, .folder]) { result in
            if case .success(let url) = result { load(path: url.path) }
        }
    }

    // MARK: Sections

    private var header: some View {
        HStack(spacing: 12) {
            Image(systemName: "square.and.arrow.down.on.square")
                .font(.system(size: 20))
                .foregroundStyle(.white)
                .frame(width: 40, height: 40)
                .background(.tint, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
            VStack(alignment: .leading, spacing: 2) {
                Text("Import from DBeaver")
                    .font(.headline)
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            Spacer()
        }
        .padding(20)
    }

    private var subtitle: String {
        guard let scan = state.value else { return source ?? "DBeaver’s saved connections" }
        let found = scan.connections.count
        let new = scan.connections.filter { !$0.alreadyAdded }.count
        let counts = "\(found) connection\(found == 1 ? "" : "s")" + (new < found ? ", \(found - new) already added" : "")
        return source.map { "\(counts) · \($0)" } ?? counts
    }

    @ViewBuilder
    private var content: some View {
        switch state {
        case .idle, .loading:
            ProgressView().controlSize(.small)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            ContentUnavailableView {
                Label("No Connections Found", systemImage: "tray")
            } description: {
                Text(message)
            } actions: {
                Button("Choose File…") { choosingFile = true }
            }
        case .loaded(let scan) where scan.connections.isEmpty && scan.skipped.isEmpty:
            ContentUnavailableView("No Connections Found", systemImage: "tray",
                                   description: Text("DBeaver has no saved connections here."))
        case .loaded(let scan):
            List {
                ForEach(groups(scan.connections), id: \.name) { group in
                    Section {
                        ForEach(group.connections) { item in
                            ImportRow(item: item, isOn: binding(for: item.id))
                        }
                    } header: {
                        if !group.name.isEmpty { Text(group.name) }
                    }
                }
                if !scan.skipped.isEmpty {
                    Section("Can’t Be Imported") {
                        ForEach(scan.skipped) { skipped in
                            LabeledContent(skipped.name) {
                                Text(skipped.reason).foregroundStyle(.secondary)
                            }
                        }
                    }
                }
            }
            .listStyle(.inset)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            Button("Choose File…") { choosingFile = true }
                .help("Import from another DBeaver data-sources.json or workspace folder")
            if let scan = state.value, !scan.connections.isEmpty {
                Button(allSelected(scan) ? "Select None" : "Select All") {
                    selected = allSelected(scan) ? [] : Set(scan.connections.map(\.id))
                }
            }
            if let importError {
                Label(importError, systemImage: "xmark.octagon.fill")
                    .foregroundStyle(.red)
                    .lineLimit(2)
                    .help(importError)
            }
            Spacer(minLength: 12)
            Button("Cancel", role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button(selected.isEmpty ? "Import" : "Import \(selected.count)") { importSelected() }
                .keyboardShortcut(.defaultAction)
                .disabled(selected.isEmpty)
        }
        .controlSize(.large)
        .padding(20)
    }

    // MARK: Helpers

    private func load(path: String?) {
        state = .loading
        source = path.map { ($0 as NSString).abbreviatingWithTildeInPath }
        do {
            let scan = try DBeaverImport.scan(path: path, existing: model.connections)
            // New connections start checked; ones already in dbear don't.
            selected = Set(scan.connections.filter { !$0.alreadyAdded }.map(\.id))
            state = .loaded(scan)
        } catch {
            state = .failed(error.localizedDescription)
        }
    }

    private func groups(_ items: [ImportedConnection]) -> [(name: String, connections: [ImportedConnection])] {
        var order: [String] = []
        var byGroup: [String: [ImportedConnection]] = [:]
        for item in items {
            if byGroup[item.config.group] == nil { order.append(item.config.group) }
            byGroup[item.config.group, default: []].append(item)
        }
        return order.map { ($0, byGroup[$0]!) }
    }

    private func allSelected(_ scan: ImportScan) -> Bool {
        selected.count == scan.connections.count
    }

    private func binding(for id: ImportedConnection.ID) -> Binding<Bool> {
        Binding(
            get: { selected.contains(id) },
            set: { on in if on { selected.insert(id) } else { selected.remove(id) } }
        )
    }

    private func importSelected() {
        guard let scan = state.value else { return }
        let chosen = scan.connections.filter { selected.contains($0.id) }
        do {
            try model.importConnections(chosen.map(\.config))
            dismiss()
        } catch {
            importError = error.localizedDescription
        }
    }
}

private struct ImportRow: View {
    let item: ImportedConnection
    @Binding var isOn: Bool

    var body: some View {
        Toggle(isOn: $isOn) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                DatabaseKindIcon(kind: item.config.kind, size: 14)
                    .foregroundStyle(.secondary)
                    .alignmentGuide(.firstTextBaseline) { $0[.bottom] - 2 }
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Text(item.config.name)
                        if item.alreadyAdded {
                            Text("Already added")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                    Text(item.config.summary)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    ForEach(item.warnings, id: \.self) { warning in
                        Label(warning, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }
        }
        .toggleStyle(.checkbox)
        .padding(.vertical, 2)
    }
}

extension AppModel {
    /// Saves imported connections (passwords go to the Keychain) and selects the first one.
    func importConnections(_ configs: [ConnectionConfig]) throws {
        var first: ConnectionConfig?
        // Imported configs have no id yet, so each one is added (never replaces an existing one).
        for config in configs {
            let saved = try save(config, password: config.password, sshSecret: config.ssh?.secret)
            first = first ?? saved
        }
        if let first { selectedConnectionID = first.id }
    }
}
