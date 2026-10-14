import DBKit
import SwiftUI

/// The command palette (⌘K): the menus' commands, the open tabs, the connections and the tables of
/// every connection listed so far, in one search. Matching and ranking are the core's
/// (`dbcore::palette`), shared with the GPUI app.
///
/// Tab on a connection (or one of its databases) connects to it and keeps the palette open on its
/// tables, shown as `[app_dev ›] search…` in the field. Backspace in the empty field, or Escape once
/// it's empty, goes back to everything.
struct CommandPalette: View {
    @Environment(AppModel.self) private var model
    @State private var query = ""
    @State private var entries: [PaletteEntry] = []
    @State private var search: PaletteSearch?
    @State private var hits: [PaletteHit] = []
    @State private var selection = 0
    /// The connection entered with Tab: only its tables are searched.
    @State private var scope: (key: DriverKey, name: String)?

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                // A database once one's entered (its tables are searched), else a magnifying glass.
                Image(systemName: scope == nil ? "magnifyingglass" : "cylinder")
                    .foregroundStyle(.secondary)
                    .frame(width: 18)
                if let scope {
                    // Part of the field, like a path: the database, then what's typed.
                    HStack(spacing: 5) {
                        Text(scope.name)
                            .font(.title3)
                        Text("/")
                            .font(.title3)
                            .foregroundStyle(.tertiary)
                    }
                    .fixedSize()
                }
                PaletteField(
                    text: $query,
                    placeholder: scope == nil ? "Search commands, tabs, connections and tables" : "Search its tables"
                ) { key in
                    switch key {
                    case .up: move(-1)
                    case .down: move(1)
                    case .submit: confirm(selection)
                    // Never moves focus out of the field.
                    case .tab: _ = enterConnection()
                    // In the empty field: leaves the entered connection (otherwise it deletes).
                    case .deleteInEmptyField:
                        guard scope != nil else { return false }
                        leaveScope()
                    // Clears the field, then leaves the entered connection, then closes.
                    case .cancel:
                        if !query.isEmpty { query = "" } else if scope != nil { leaveScope() } else { close() }
                    }
                    return true
                }
                if tablesState == .loading {
                    ProgressView().controlSize(.small)
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            Divider()
            if hits.isEmpty {
                Group {
                    switch tablesState {
                    case .loading:
                        HStack(spacing: 8) {
                            ProgressView().controlSize(.small)
                            Text("Loading tables…")
                        }
                    case .failed(let error): Text(error).foregroundStyle(.red)
                    case .ready where query.isEmpty: Text("No tables")
                    default: Text("No matches")
                    }
                }
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 22)
                .padding(.horizontal, 14)
            } else {
                list
            }
            Divider()
            HStack(spacing: 12) {
                if scope != nil {
                    if tablesState == .ready { Text("\(entries.count) tables") }
                    Spacer()
                    Text("↩ open  ⌫ everything")
                } else {
                    Text("> commands")
                    Text("# tables")
                    Text("@ connections")
                    Spacer()
                    Text("⇥ a connection’s tables  ↩ open")
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
        }
        .frame(width: 600)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(.separator))
        .shadow(color: .black.opacity(0.25), radius: 24, y: 10)
        .onAppear {
            load(model.paletteEntries())
        }
        .onChange(of: query) { rank() }
        // An entered connection's tables, once they're listed.
        .onChange(of: tablesState) {
            if let scope { load(model.paletteTableEntries(scope.key)) }
        }
    }

    private var list: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 0) {
                    ForEach(Array(hits.enumerated()), id: \.offset) { index, hit in
                        PaletteRow(entry: entries[hit.index], positions: hit.positions, selected: index == selection)
                            .id(index)
                            .contentShape(Rectangle())
                            .onTapGesture { confirm(index) }
                            .onHover { if $0 { selection = index } }
                    }
                }
                .padding(6)
            }
            .frame(maxHeight: 380)
            .fixedSize(horizontal: false, vertical: true)
            .onChange(of: selection) { proxy.scrollTo(selection) }
        }
    }

    private func load(_ newEntries: [PaletteEntry]) {
        entries = newEntries
        search = PaletteSearch(entries.map(\.item))
        rank()
    }

    /// The entered connection's tables: loading, listed, or why not (`nil`: none entered).
    private var tablesState: PaletteTablesState? {
        scope.map { model.paletteTablesState($0.key) }
    }

    private func rank() {
        if scope != nil {
            // Its tables only, all of them before anything's typed.
            hits = search?.rank("#" + query.drop { "#>@".contains($0) }) ?? []
        } else {
            hits = search?.rank(query) ?? []
        }
        selection = 0
    }

    /// Tab: connects to the selected connection or database and lists its tables.
    private func enterConnection() -> Bool {
        guard hits.indices.contains(selection),
              case .connection(let id, let database) = entries[hits[selection].index].target,
              let (key, name) = model.paletteEnter(id, database: database) else { return false }
        scope = (key, name)
        query = ""
        load(model.paletteTableEntries(key))
        return true
    }

    private func leaveScope() {
        scope = nil
        query = ""
        // Connecting may have made more commands and tables available.
        load(model.paletteEntries())
    }

    private func move(_ offset: Int) {
        guard !hits.isEmpty else { return }
        selection = min(max(selection + offset, 0), hits.count - 1)
    }

    private func confirm(_ index: Int) {
        guard hits.indices.contains(index) else { return }
        model.perform(entries[hits[index].index].target)
    }

    private func close() {
        model.showingPalette = false
    }
}

private struct PaletteRow: View {
    let entry: PaletteEntry
    let positions: [Int]
    let selected: Bool

    var body: some View {
        HStack(spacing: 8) {
            Group {
                switch entry.icon {
                case .symbol(let name): Image(systemName: name)
                case .kind(let kind): DatabaseKindIcon(kind: kind, size: 14)
                }
            }
            .frame(width: 18)
            .foregroundStyle(.secondary)
            Text(highlighted)
                .lineLimit(1)
                .layoutPriority(1)
            if let detail = entry.detail {
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if let shortcut = entry.shortcut {
                Text(shortcut)
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .background(selected ? Color.accentColor.opacity(0.22) : .clear, in: RoundedRectangle(cornerRadius: 6))
    }

    /// The label with the characters the query matched in bold.
    private var highlighted: AttributedString {
        let label = entry.item.label
        var text = AttributedString(label)
        for offset in positions {
            let start = String.Index(utf16Offset: offset, in: label)
            guard start < label.endIndex else { continue }
            let end = label.unicodeScalars.index(after: start)
            guard let lower = AttributedString.Index(start, within: text),
                  let upper = AttributedString.Index(end, within: text) else { continue }
            text[lower..<upper].inlinePresentationIntent = .stronglyEmphasized
        }
        return text
    }
}

/// The palette's search field. An AppKit field, so its keys (↑ ↓, ⌃N ⌃P, Tab, Return, Escape,
/// Backspace in the empty field) reach the palette before the text system acts on them (Tab would
/// move focus out of a SwiftUI `TextField`).
private struct PaletteField: NSViewRepresentable {
    enum Key { case up, down, tab, submit, cancel, deleteInEmptyField }

    @Binding var text: String
    let placeholder: String
    /// Returns whether it handled the key.
    let onKey: (Key) -> Bool

    func makeNSView(context: Context) -> NSTextField {
        let field = NSTextField()
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .preferredFont(forTextStyle: .title3)
        field.cell?.usesSingleLineMode = true
        field.cell?.isScrollable = true
        field.delegate = context.coordinator
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        DispatchQueue.main.async { field.window?.makeFirstResponder(field) }
        return field
    }

    func updateNSView(_ field: NSTextField, context: Context) {
        context.coordinator.parent = self
        if field.stringValue != text { field.stringValue = text }
        field.placeholderString = placeholder
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    final class Coordinator: NSObject, NSTextFieldDelegate {
        var parent: PaletteField

        init(_ parent: PaletteField) { self.parent = parent }

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            parent.text = field.stringValue
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            switch selector {
            // ⌃P / ⌃N are bound to these by the text system too.
            case #selector(NSResponder.moveUp(_:)): parent.onKey(.up)
            case #selector(NSResponder.moveDown(_:)): parent.onKey(.down)
            case #selector(NSResponder.insertTab(_:)): parent.onKey(.tab)
            case #selector(NSResponder.insertNewline(_:)): parent.onKey(.submit)
            case #selector(NSResponder.cancelOperation(_:)): parent.onKey(.cancel)
            case #selector(NSResponder.deleteBackward(_:)) where textView.string.isEmpty: parent.onKey(.deleteInEmptyField)
            default: false
            }
        }
    }
}
