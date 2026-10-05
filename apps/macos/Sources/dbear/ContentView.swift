import SwiftUI

struct ContentView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        NavigationSplitView {
            ConnectionsSidebar()
                .navigationSplitViewColumnWidth(min: 200, ideal: 240, max: 320)
        } content: {
            TablesList()
                .navigationSplitViewColumnWidth(min: 240, ideal: 300, max: 420)
        } detail: {
            WorkspaceView()
        }
        .separatorColoredSplitDividers()
        .task { await model.monitorConnections() }
        .sheet(item: $model.editor) { request in
            ConnectionEditor(original: request.original)
        }
        .sheet(isPresented: $model.showingImport) {
            ImportConnectionsSheet()
        }
        .sheet(item: $model.dumpRequest) { DumpSheet(request: $0) }
        .sheet(item: $model.restoreRequest) { RestoreSheet(request: $0) }
        .usersSheets(model)
        .overlay(alignment: .bottomTrailing) { BackupJobsPanel() }
        .confirmationDialog(
            "Delete “\(model.pendingDeletion?.name ?? "")”?",
            isPresented: Binding(get: { model.pendingDeletion != nil }, set: { if !$0 { model.pendingDeletion = nil } }),
            presenting: model.pendingDeletion
        ) { connection in
            Button("Delete", role: .destructive) { model.delete(connection) }
            Button("Cancel", role: .cancel) {}
        } message: { _ in
            Text("Its saved password is removed from the Keychain and its open tabs are closed.")
        }
    }
}

/// The big faded placeholder Mail shows ("No Message Selected").
struct EmptyPlaceholder: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.system(size: 26))
            .foregroundStyle(.tertiary)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct RefreshButton: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Button {
            Task { await model.refreshActiveTab() }
        } label: {
            Label("Refresh", systemImage: "arrow.clockwise")
        }
        .disabled(model.activeTab == nil)
        .help("Reload")
    }
}
