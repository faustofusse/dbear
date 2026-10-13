import DBKit
import SwiftUI

/// The command palette (⌘K): the menus' commands, the open tabs, the connections and the tables of
/// every connection listed so far, in one search. Matching and ranking are the core's
/// (`dbcore::palette`), shared with the GPUI app.
struct CommandPalette: View {
    @Environment(AppModel.self) private var model
    @State private var query = ""
    @State private var entries: [PaletteEntry] = []
    @State private var search: PaletteSearch?
    @State private var hits: [PaletteHit] = []
    @State private var selection = 0
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(.secondary)
                TextField("Search commands, tabs, connections and tables", text: $query)
                    .textFieldStyle(.plain)
                    .font(.title3)
                    .focused($focused)
                    .onSubmit { confirm(selection) }
                    .onKeyPress(.upArrow) { move(-1); return .handled }
                    .onKeyPress(.downArrow) { move(1); return .handled }
                    // ⌃N / ⌃P too, like the text system's own (and most palettes).
                    .onKeyPress(keys: ["n", "p"]) { press in
                        guard press.modifiers == .control else { return .ignored }
                        move(press.key == "n" ? 1 : -1)
                        return .handled
                    }
                    .onExitCommand { close() }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            Divider()
            if hits.isEmpty {
                Text("No matches")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 22)
            } else {
                list
            }
            Divider()
            HStack(spacing: 12) {
                Text("> commands")
                Text("# tables")
                Text("@ connections")
                Spacer()
                Text("↑↓ move  ↩ open  ⎋ close")
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
            entries = model.paletteEntries()
            search = PaletteSearch(entries.map(\.item))
            rank()
            focused = true
        }
        .onChange(of: query) { rank() }
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

    private func rank() {
        hits = search?.rank(query) ?? []
        selection = 0
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
