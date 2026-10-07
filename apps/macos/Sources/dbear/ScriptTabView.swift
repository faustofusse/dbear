import DBKit
import SwiftUI

/// SQL editor on top, results below, resizable. A results tab shows only the results.
struct ScriptTabView: View {
    @Environment(AppModel.self) private var model
    @Bindable var tab: ScriptTab

    var body: some View {
        Group {
            if tab.isResults {
                VStack(spacing: 0) {
                    resultsBar
                    Divider()
                    results
                }
            } else {
                scriptBody
            }
        }
        .sheet(isPresented: Binding(get: { tab.isReviewingEdits }, set: { tab.isReviewingEdits = $0 })) {
            ReviewChangesSheet(tab: tab)
        }
    }

    private var scriptBody: some View {
        // Split position lives on the tab, so it survives switching tabs.
        VerticalSplit(topHeight: $tab.editorHeight, minTop: 120, minBottom: 150) {
            VStack(spacing: 0) {
                editorBar
                SQLEditor(
                    text: $tab.text,
                    focusOnAppear: consumeInitialFocus(),
                    fontSize: model.editorFontSize,
                    onZoomIn: { model.zoomEditor(by: 1) },
                    initialSelection: tab.selectedRanges,
                    onSelectionChange: { [tab] in tab.updateSelection($0) },
                    completionCatalog: model.completionCatalog(for: tab.connection),
                    databaseKind: tab.connection.kind
                )
            }
        } bottom: {
            results
        }
        .onAppear { model.loadCompletionCatalogIfNeeded(for: tab.connection) }
    }

    /// New scripts start focused with the caret at the end (only read when the editor is created).
    private func consumeInitialFocus() -> Bool {
        guard tab.needsInitialFocus else { return false }
        // Mutating observed state during body evaluation would re-render; defer it.
        DispatchQueue.main.async { tab.needsInitialFocus = false }
        return true
    }

    private var connectionLabel: some View {
        Label {
            Text(model.displayName(of: tab.connection))
        } icon: {
            DatabaseKindIcon(kind: tab.connection.kind, size: 13)
        }
            .font(.callout)
            .foregroundStyle(.secondary)
            .help(tab.connection.summary)
            .fixedSize()
    }

    private var stopButton: some View {
        Button {
            Task { await model.cancel(tab) }
        } label: {
            Label("Stop", systemImage: "stop.fill")
        }
        .keyboardShortcut(".", modifiers: .command)
        .help("Stop Script (⌘.)")
    }

    /// A results tab's bar: the SQL it ran, and Re-run.
    private var resultsBar: some View {
        HStack(spacing: 8) {
            connectionLabel
            Text(tab.text.split(whereSeparator: \.isWhitespace).joined(separator: " "))
                .font(.system(.callout, design: .monospaced))
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .truncationMode(.tail)
                .help(tab.text)
                .frame(maxWidth: .infinity, alignment: .leading)
            if tab.result.isLoading {
                ProgressView().controlSize(.small)
                stopButton
            } else {
                Button {
                    Task { await model.run(tab) }
                } label: {
                    Label("Re-run", systemImage: "arrow.clockwise")
                }
                .keyboardShortcut(.return, modifiers: .command)
                .help("Run the Query Again (⌘↩)")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    private var editorBar: some View {
        HStack(spacing: 8) {
            connectionLabel
            Spacer()
            if tab.hasRows && !tab.result.isLoading {
                Button {
                    model.openResultsInNewTab(tab)
                } label: {
                    Label("Open in New Tab", systemImage: "arrow.up.right.square")
                        .labelStyle(.iconOnly)
                }
                .buttonStyle(.borderless)
                .help("Open These Results in a New Tab")
            }
            Button {
                model.runInNewTab(tab)
            } label: {
                Label(tab.hasSelection ? "Run Selection in New Tab" : "Run in New Tab", systemImage: "plus.rectangle.on.rectangle")
                    .labelStyle(.iconOnly)
            }
            .buttonStyle(.borderless)
            .keyboardShortcut(.return, modifiers: [.command, .shift])
            .help(tab.hasSelection ? "Run Selected SQL in a New Tab (⇧⌘↩)" : "Run Script in a New Tab (⇧⌘↩)")
            if tab.result.isLoading {
                ProgressView().controlSize(.small)
                stopButton
            } else {
                Button {
                    Task { await model.run(tab) }
                } label: {
                    Label(tab.hasSelection ? "Run Selection" : "Run", systemImage: "play.fill")
                }
                .keyboardShortcut(.return, modifiers: .command)
                .help(tab.hasSelection ? "Run Selected SQL (⌘↩)" : "Run Script (⌘↩)")
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    @ViewBuilder
    private var results: some View {
        switch tab.result {
        case .idle:
            Text(tab.wasCancelled ? "Query cancelled" : tab.isResults ? "Not run yet" : "Press ⌘↩ to run")
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .loading:
            ProgressView().controlSize(.small)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .failed(let message):
            ContentUnavailableView {
                Label("Query Failed", systemImage: "exclamationmark.triangle")
            } description: {
                Text(message)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
            }
        case .loaded(let result) where result.columns.isEmpty:
            // INSERT/UPDATE/DDL: nothing to show in a grid.
            VStack(spacing: 6) {
                Text(result.rowsAffected.map { "\($0.formatted()) \($0 == 1 ? "row" : "rows") affected" } ?? "Done")
                    .font(.title3)
                if let duration = tab.lastDuration {
                    Text(duration.formatted(.units(allowed: [.seconds, .milliseconds], width: .narrow)))
                        .foregroundStyle(.secondary)
                }
            }
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .loaded(let result):
            DataGrid(
                result: result, version: tab.runCount, duration: tab.lastDuration,
                editing: editing,
                foreignKeys: tab.sources.map { sources in
                    GridForeignKeys(
                        sources: sources,
                        open: { model.openReferencedRow($0, values: $1, from: tab.connection) },
                        openReferencing: { model.openReferencingRows($0, values: $1, from: tab.connection) }
                    )
                },
                focus: GridFocus(
                    initial: { tab.focusedCell },
                    changed: { tab.focusedCell = $0 },
                    inspect: { model.showsInspector = true }
                ),
                source: GridSource(kind: tab.connection.kind)
            )
        }
    }

    /// Cells of tables whose primary key is in the results can be edited (see `ResultSources`).
    private var editing: GridEditing? {
        guard let sources = tab.sources, sources.isEditable else { return nil }
        return GridEditing(
            edits: tab.edits,
            editRequest: tab.editRequest,
            setCell: { model.setCell(tab, row: $0, column: $1, to: $2) },
            addRow: {},
            deleteRows: { model.deleteRows(tab, ids: $0) },
            revertRows: { model.revertRows(tab, ids: $0) },
            selectionChanged: { tab.selectedRowIDs = $0 },
            requestHandled: { tab.editRequest = nil },
            columnReadOnly: { sources.readOnlyReason(column: $0) },
            canAddRows: false,
            canDeleteRows: sources.canDeleteRows
        )
    }
}
