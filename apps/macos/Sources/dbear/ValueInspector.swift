import AppKit
import DBKit
import SwiftUI

/// The active tab's focused grid cell, as the inspector shows it.
struct InspectedCell {
    let column: ColumnInfo
    /// What the grid shows: the unsaved edit if there is one, else the loaded value. `nil` is NULL.
    let text: String?
    /// The cell holds an unsaved DEFAULT.
    let isDefault: Bool
    /// "Row 12", "New row".
    let rowLabel: String
    /// Changes whenever another cell (or a new value of the same cell) is shown, resetting the draft.
    let identity: String
    /// Writes the cell (as a pending edit) when it can be edited.
    let apply: ((EditValue) -> Void)?
    /// Why it can't be edited.
    let readOnlyReason: String?
}

extension AppModel {
    var inspectedCell: InspectedCell? {
        switch activeTab {
        case .table(let tab): inspected(tab)
        case .script(let tab): inspected(tab)
        case .users, nil: nil
        }
    }

    private func inspected(_ tab: TableTab) -> InspectedCell? {
        guard tab.mode == .data, let result = tab.data.value, let address = tab.focusedCell,
              let column = result.columns[safe: address.column]
        else { return nil }
        let id = address.row
        let loaded: DBValue
        let rowLabel: String
        if id < 0 {
            guard let position = tab.edits.inserted.firstIndex(where: { $0.id == id }) else { return nil }
            loaded = .null
            rowLabel = tab.edits.inserted.count == 1 ? "New row" : "New row \(position + 1)"
        } else {
            guard let index = result.rows.firstIndex(where: { $0.id == id }) else { return nil }
            loaded = result.rows[index].values[safe: address.column] ?? .null
            rowLabel = "Row \((index + 1).formatted())"
        }
        let edit = tab.edits.value(row: id, column: address.column)
        let text: String? = switch edit {
        case .text(let t)?: t
        case .null?, .default?: nil
        case nil: loaded.isNull ? nil : loaded.displayString
        }
        let reason: String? = tab.readOnlyReason
            ?? (column.isBinary ? "Binary values can’t be edited here." : nil)
            ?? (tab.edits.deleted.contains(id) ? "This row will be deleted." : nil)
        return InspectedCell(
            column: column, text: text, isDefault: edit == .default, rowLabel: rowLabel,
            identity: "\(tab.id)/\(tab.dataVersion)/\(id)/\(address.column)/\(text ?? "\u{0}")/\(edit == .default)",
            apply: reason == nil ? { [weak self] in self?.setCell(tab, row: id, column: address.column, to: $0) } : nil,
            readOnlyReason: reason
        )
    }

    private func inspected(_ tab: ScriptTab) -> InspectedCell? {
        guard let result = tab.result.value, let address = tab.focusedCell,
              let column = result.columns[safe: address.column],
              let index = result.rows.firstIndex(where: { $0.id == address.row })
        else { return nil }
        let id = address.row
        let loaded = result.rows[index].values[safe: address.column] ?? .null
        let edit = tab.edits.value(row: id, column: address.column)
        let text: String? = switch edit {
        case .text(let t)?: t
        case .null?, .default?: nil
        case nil: loaded.isNull ? nil : loaded.displayString
        }
        // Cells of a table whose primary key is in the results can be edited (see `ResultSources`).
        let reason: String? = tab.columnReadOnly(address.column)
            ?? (column.isBinary ? "Binary values can’t be edited here." : nil)
            ?? (tab.edits.deleted.contains(id) ? "This row will be deleted." : nil)
        return InspectedCell(
            column: column, text: text, isDefault: edit == .default,
            rowLabel: "Row \((index + 1).formatted())",
            identity: "\(tab.id)/\(tab.runCount)/\(id)/\(address.column)/\(text ?? "\u{0}")/\(edit == .default)",
            apply: reason == nil ? { [weak self] in self?.setCell(tab, row: id, column: address.column, to: $0) } : nil,
            readOnlyReason: reason
        )
    }
}

/// Right-hand inspector (⌘I): the focused cell's full value. JSON is pretty-printed, long text
/// wraps, and cells of editable tables can be edited here (staged like grid edits).
struct ValueInspector: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        if let cell = model.inspectedCell {
            InspectorContent(cell: cell).id(cell.identity)
        } else {
            Text("No Cell Selected")
                .font(.title3)
                .foregroundStyle(.tertiary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }
}

private struct InspectorContent: View {
    let cell: InspectedCell
    /// Pretty-printed JSON, when the value is a JSON object or array.
    private let pretty: String?
    @State private var showsPretty: Bool
    @State private var draft: String

    init(cell: InspectedCell) {
        self.cell = cell
        let pretty = cell.text.flatMap(RowFormatter.prettyJSON)
        self.pretty = pretty
        _showsPretty = State(initialValue: pretty != nil)
        _draft = State(initialValue: pretty ?? cell.text ?? "")
    }

    /// The value as currently presented (pretty or raw), which the draft is compared against.
    private var presented: String { (showsPretty ? pretty : nil) ?? cell.text ?? "" }
    private var isChanged: Bool { draft != presented || (cell.text == nil && !draft.isEmpty) }
    private var isEditable: Bool { cell.apply != nil }
    private var isTruncatedBinary: Bool { cell.column.isBinary && (cell.text?.hasSuffix("…") ?? false) }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            InspectorTextView(text: $draft, isEditable: isEditable, highlightsJSON: pretty != nil)
                .overlay(alignment: .topLeading) {
                    if draft.isEmpty {
                        Text(cell.isDefault ? "DEFAULT" : cell.text == nil ? "NULL" : "Empty")
                            .font(.system(size: 12, design: .monospaced).italic())
                            .foregroundStyle(.tertiary)
                            .padding(.horizontal, 13)
                            .padding(.vertical, 8)
                            .allowsHitTesting(false)
                    }
                }
            footer
        }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 2) {
                Text(cell.column.name)
                    .font(.headline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if pretty != nil {
                Picker("Format", selection: $showsPretty) {
                    Text("Pretty").tag(true)
                    Text("Raw").tag(false)
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                .disabled(isChanged)
                .help(isChanged ? "Apply or revert your changes to switch formats" : "Show the JSON indented or as stored")
                .onChange(of: showsPretty) { draft = presented }
            }
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(draft, forType: .string)
            } label: {
                Image(systemName: "doc.on.doc")
            }
            .buttonStyle(.borderless)
            .disabled(cell.text == nil && draft.isEmpty)
            .help("Copy Value")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
    }

    /// "text · Row 12 · primary key".
    private var subtitle: String {
        var parts = [cell.column.typeName.isEmpty ? nil : cell.column.typeName, cell.rowLabel]
        if cell.column.isPrimaryKey { parts.append("primary key") }
        return parts.compactMap { $0 }.joined(separator: " · ")
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if isEditable {
                HStack {
                    if cell.column.isNullable {
                        Button("Set to NULL") { cell.apply?(.null) }
                            .disabled(cell.text == nil && !cell.isDefault)
                    }
                    Spacer()
                    Button("Revert") { draft = presented }
                        .disabled(!isChanged)
                    Button("Apply") { cell.apply?(.text(draft)) }
                        .keyboardShortcut(.return, modifiers: .command)
                        .buttonStyle(.borderedProminent)
                        .disabled(!isChanged)
                        .help("Stage the new value (⌘↩); save it with the table’s other changes")
                }
                .controlSize(.small)
            }
            HStack(spacing: 6) {
                Text(stats)
                Spacer()
                if let reason = cell.readOnlyReason, cell.apply == nil {
                    Image(systemName: "lock").help(reason)
                }
            }
            .font(.callout)
            .foregroundStyle(.secondary)
            .monospacedDigit()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(.bar)
        .overlay(alignment: .top) { Divider() }
    }

    private var stats: String {
        if cell.isDefault, !isChanged { return "DEFAULT" }
        guard let text = cell.text ?? (draft.isEmpty ? nil : draft) else { return "NULL" }
        if cell.column.isBinary {
            let bytes = max(0, text.count - 2) / 2
            return isTruncatedBinary ? "First \(bytes.formatted()) bytes" : "\(bytes.formatted()) bytes"
        }
        let value = isChanged ? draft : text
        let characters = value.count
        let bytes = value.utf8.count
        let lines = value.reduce(1) { $1.isNewline ? $0 + 1 : $0 }
        var parts = [characters == 1 ? "1 character" : "\(characters.formatted()) characters"]
        if bytes != characters { parts.append("\(bytes.formatted()) bytes") }
        if lines > 1 { parts.append("\(lines.formatted()) lines") }
        return parts.joined(separator: " · ")
    }
}

/// Monospaced text view that wraps lines and handles large values (an `NSTextView`).
/// JSON values get tree-sitter highlighting from the core, refreshed as they're edited.
private struct InspectorTextView: NSViewRepresentable {
    @Binding var text: String
    var isEditable: Bool
    var highlightsJSON: Bool

    func makeCoordinator() -> Coordinator { Coordinator(text: $text, highlightsJSON: highlightsJSON) }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        scroll.drawsBackground = false
        scroll.useThinScrollers()
        let view = scroll.documentView as! NSTextView
        view.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        view.textColor = .labelColor
        view.drawsBackground = false
        view.isRichText = false
        view.importsGraphics = false
        view.allowsUndo = true
        view.usesFindBar = true
        view.isIncrementalSearchingEnabled = true
        view.textContainerInset = NSSize(width: 8, height: 8)
        view.isAutomaticQuoteSubstitutionEnabled = false
        view.isAutomaticDashSubstitutionEnabled = false
        view.isAutomaticTextReplacementEnabled = false
        view.isAutomaticSpellingCorrectionEnabled = false
        view.isContinuousSpellCheckingEnabled = false
        view.isGrammarCheckingEnabled = false
        view.smartInsertDeleteEnabled = false
        view.string = text
        view.delegate = context.coordinator
        context.coordinator.textView = view
        context.coordinator.highlight()
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let view = scroll.documentView as? NSTextView else { return }
        let coordinator = context.coordinator
        coordinator.text = $text
        var needsHighlight = coordinator.highlightsJSON != highlightsJSON
        coordinator.highlightsJSON = highlightsJSON
        if view.string != text {
            view.string = text
            needsHighlight = true
        }
        if needsHighlight { coordinator.highlight() }
        if view.isEditable != isEditable { view.isEditable = isEditable }
        view.isSelectable = true
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var text: Binding<String>
        var highlightsJSON: Bool
        weak var textView: NSTextView?
        private let theme = SQLTheme.json(fontSize: 12)
        private var generation = 0

        init(text: Binding<String>, highlightsJSON: Bool) {
            self.text = text
            self.highlightsJSON = highlightsJSON
        }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? NSTextView else { return }
            text.wrappedValue = view.string
            highlight()
        }

        /// Small values synchronously; large ones off the main thread, applied only if the
        /// text hasn't changed meanwhile (as in `SQLEditor`).
        func highlight() {
            guard let textView else { return }
            generation += 1
            textView.typingAttributes = theme.baseAttributes
            guard highlightsJSON else {
                apply([], to: textView)
                return
            }
            let source = textView.string
            if source.utf16.count < 50_000 {
                apply(JSONSyntax.highlight(source), to: textView)
                return
            }
            let generation = generation
            Task.detached(priority: .userInitiated) {
                let spans = JSONSyntax.highlight(source)
                await MainActor.run { [weak self] in
                    guard let self, self.generation == generation, let textView = self.textView else { return }
                    self.apply(spans, to: textView)
                }
            }
        }

        private func apply(_ spans: [SyntaxSpan], to textView: NSTextView) {
            guard let storage = textView.textStorage else { return }
            let length = storage.length
            storage.beginEditing()
            storage.setAttributes(theme.baseAttributes, range: NSRange(location: 0, length: length))
            for span in spans where NSMaxRange(span.range) <= length {
                storage.addAttributes(theme.attributes(for: span.kind), range: span.range)
            }
            storage.endEditing()
        }
    }
}
