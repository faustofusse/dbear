import AppKit
import DBKit
import SwiftUI

/// Right-hand pane: a tab strip with table and SQL script tabs.
struct WorkspaceView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        content
            .toolbar {
                // [New Script]  [Data | Structure]  [+ −] ……… [Discard] [Review] [Save]  [Refresh]
                ToolbarItem { newScriptButton }
                if let tab = model.activeTableTab {
                    if #available(macOS 26.0, *) {
                        ToolbarSpacer(.fixed)
                    }
                    ToolbarItem { TableModePicker(tab: tab) }
                    if tab.mode == .data {
                        if #available(macOS 26.0, *) {
                            ToolbarSpacer(.fixed)
                        }
                        // One capsule for both, like the pending-changes buttons.
                        ToolbarItemGroup { RowToolbarButtons(tab: tab) }
                    }
                }
                if #available(macOS 26.0, *) {
                    ToolbarSpacer(.flexible)
                }
                if let tab = model.activeTableTab, !tab.edits.isEmpty {
                    ToolbarItemGroup { PendingChangesButtons(tab: tab) }
                    if #available(macOS 26.0, *) {
                        ToolbarSpacer(.fixed)
                    }
                }
                ToolbarItem { RefreshButton() }
            }
    }

    private var newScriptButton: some View {
        Button {
            model.newScript()
        } label: {
            Label("New SQL Script", systemImage: "square.and.pencil")
        }
        .keyboardShortcut("t", modifiers: .command)
        .disabled(model.selectedConnection == nil)
        .help("New SQL Script (⌘T)")
    }

    @ViewBuilder
    private var content: some View {
        if model.tabs.isEmpty {
            EmptyPlaceholder(text: "No Table Selected")
        } else {
            VStack(spacing: 0) {
                // Like Safari: no tab bar for a single tab (the toolbar still has New SQL Script, ⌘T).
                if model.tabs.count > 1 {
                    TabStrip()
                }
                Group {
                    switch model.activeTab {
                    case .table(let tab): TableTabView(tab: tab).id(tab.id)
                    case .script(let tab): ScriptTabView(tab: tab).id(tab.id)
                    case nil: EmptyPlaceholder(text: "No Tab Selected")
                    }
                }
                // Whatever a tab shows (even a small error message) fills the pane,
                // so the tab strip always stays at the top.
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            // Tab switches, opens and closes are instant: no implicit or inherited animations.
            .transaction { $0.disablesAnimations = true; $0.animation = nil }
        }
    }
}

// MARK: - Tab strip (Finder / Safari style)

private struct TabStrip: View {
    @Environment(AppModel.self) private var model
    @State private var hoveredID: UUID?
    /// Frame of each tab in the `tabs` coordinate space. It includes the drag offset, so a drag
    /// works from the snapshot it took when it started (`TabDrag.frames`).
    @State private var frames: [UUID: CGRect] = [:]
    @State private var drag: TabDrag?
    @State private var middleClickMonitor: Any?

    private static let space = "tabs"

    /// A tab being dragged to reorder it. Tabs are only reordered on drop; during the drag the
    /// other tabs just slide aside with offsets, measured against the frames at drag start.
    private struct TabDrag {
        let id: UUID
        let from: Int
        let frames: [UUID: CGRect]
        var translation: CGFloat
        var target: Int
    }

    var body: some View {
        HStack(spacing: 8) {
            HStack(spacing: 0) {
                ForEach(Array(model.tabs.enumerated()), id: \.element.id) { index, tab in
                    if index > 0 {
                        TabSeparator(hidden: isHighlighted(model.tabs[index - 1].id) || isHighlighted(tab.id))
                    }
                    TabItem(
                        tab: tab,
                        isActive: tab.id == model.activeTabID,
                        hovered: Binding(
                            get: { hoveredID == tab.id },
                            set: { hoveredID = $0 ? tab.id : (hoveredID == tab.id ? nil : hoveredID) }
                        )
                    )
                    .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Self.space)) } action: {
                        frames[tab.id] = $0
                    }
                    // While dragged, the tab stays in its slot (it owns the gesture) but is drawn by
                    // the overlay below, above the tabs it passes over.
                    .opacity(drag?.id == tab.id ? 0 : 1)
                    .offset(x: dragOffset(of: tab.id))
                    .gesture(dragGesture(for: tab.id))
                }
            }
            .coordinateSpace(.named(Self.space))
            .overlay(alignment: .topLeading) { draggedTab }
            .padding(2)
            .frame(height: 30)
            .background(Capsule().fill(.primary.opacity(0.06)))

            NewTabButton()
        }
        .padding(.horizontal, 10)
        .padding(.top, 4)
        .padding(.bottom, 6)
        .onAppear(perform: installMiddleClickMonitor)
        .onDisappear {
            if let middleClickMonitor { NSEvent.removeMonitor(middleClickMonitor) }
            middleClickMonitor = nil
        }
    }

    private func isHighlighted(_ id: UUID) -> Bool {
        id == model.activeTabID || id == hoveredID
    }

    // MARK: Drag to reorder

    /// The dragged tab, drawn on top of the strip at the pointer. Its own fill is translucent,
    /// so it gets an opaque backing to hide the tabs underneath.
    @ViewBuilder
    private var draggedTab: some View {
        if let drag, let tab = model.tabs.first(where: { $0.id == drag.id }), let frame = drag.frames[drag.id] {
            TabItem(tab: tab, isActive: tab.id == model.activeTabID, hovered: .constant(true))
                .frame(width: frame.width, height: frame.height)
                .background(Capsule().fill(Color(nsColor: .windowBackgroundColor)))
                .offset(x: frame.minX + drag.translation, y: frame.minY)
                .allowsHitTesting(false)
        }
    }

    /// The tabs the dragged one has passed shift one slot toward its original position.
    private func dragOffset(of id: UUID) -> CGFloat {
        guard let drag, let dragged = drag.frames[drag.id] else { return 0 }
        if id == drag.id { return 0 }
        guard let index = model.tabs.firstIndex(where: { $0.id == id }) else { return 0 }
        let step = dragged.width + separatorWidth(drag.frames)
        if drag.from < index, index <= drag.target { return -step }
        if drag.target <= index, index < drag.from { return step }
        return 0
    }

    /// Space between two adjacent tabs (the separator).
    private func separatorWidth(_ frames: [UUID: CGRect]) -> CGFloat {
        guard model.tabs.count > 1, let a = frames[model.tabs[0].id], let b = frames[model.tabs[1].id] else { return 1 }
        return max(0, b.minX - a.maxX)
    }

    private func dragGesture(for id: UUID) -> some Gesture {
        DragGesture(minimumDistance: 4, coordinateSpace: .global)
            .onChanged { value in
                if drag?.id != id {
                    guard let from = model.tabs.firstIndex(where: { $0.id == id }) else { return }
                    drag = TabDrag(id: id, from: from, frames: frames, translation: 0, target: from)
                    model.activate(id)
                }
                guard let from = drag?.from, let frames = drag?.frames, let frame = frames[id] else { return }
                // Keep the tab inside the strip.
                var translation = value.translation.width
                if let first = model.tabs.first.flatMap({ frames[$0.id] }) {
                    translation = max(translation, first.minX - frame.minX)
                }
                if let last = model.tabs.last.flatMap({ frames[$0.id] }) {
                    translation = min(translation, last.maxX - frame.maxX)
                }
                // Target slot: how many other tabs the dragged tab's center has passed.
                let center = frame.midX + translation
                let target = model.tabs.enumerated().filter { index, tab in
                    index != from && (frames[tab.id]?.midX ?? .infinity) < center
                }.count
                drag?.translation = translation
                drag?.target = target
            }
            .onEnded { _ in
                if let drag { model.moveTab(drag.id, to: drag.target) }
                drag = nil
            }
    }

    // MARK: Middle click closes

    /// SwiftUI has no middle-click gesture, so watch for it and close the tab under the pointer.
    private func installMiddleClickMonitor() {
        guard middleClickMonitor == nil else { return }
        middleClickMonitor = NSEvent.addLocalMonitorForEvents(matching: .otherMouseUp) { [model] event in
            guard event.buttonNumber == 2, let id = hoveredID,
                  model.tabs.contains(where: { $0.id == id }) else { return event }
            hoveredID = nil
            model.requestClose(id)
            return nil
        }
    }
}

private struct TabSeparator: View {
    let hidden: Bool

    var body: some View {
        Rectangle()
            .fill(.primary.opacity(0.12))
            .frame(width: 1, height: 14)
            .opacity(hidden ? 0 : 1)
    }
}

private struct TabItem: View {
    @Environment(AppModel.self) private var model
    let tab: WorkspaceTab
    let isActive: Bool
    @Binding var hovered: Bool

    var body: some View {
        ZStack {
            HStack(spacing: 5) {
                Image(systemName: tab.systemImage)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                Text(tab.title)
                    .italic(tab.isPreview)
                    .fontWeight(isActive ? .semibold : .regular)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .padding(.horizontal, 26)

            HStack {
                CloseButton { model.requestClose(tab.id) }
                    .opacity(hovered ? 1 : 0)
                Spacer(minLength: 0)
            }
            .padding(.leading, 5)
        }
        .font(.system(size: 13))
        .foregroundStyle(isActive ? .primary : .secondary)
        .frame(minWidth: 80, maxWidth: .infinity, maxHeight: .infinity)
        .background {
            if isActive {
                Capsule()
                    .fill(.primary.opacity(0.14))
                    .overlay(Capsule().strokeBorder(.primary.opacity(0.08), lineWidth: 0.5))
                    .shadow(color: .black.opacity(0.15), radius: 1, y: 0.5)
            } else if hovered {
                Capsule().fill(.primary.opacity(0.05))
            }
        }
        .contentShape(Capsule())
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(tab.title)
        .accessibilityAddTraits(isActive ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction { model.activate(tab.id) }
        .onHover { hovered = $0 }
        .onTapGesture { model.activate(tab.id) }
        .simultaneousGesture(TapGesture(count: 2).onEnded { model.pin(tab.id) })
        .help("\(model.displayName(of: tab.connection)) · \(tooltip)")
        .contextMenu {
            if tab.isPreview {
                Button("Keep Open") { model.pin(tab.id) }
            }
            Button("Close Tab") { model.requestClose(tab.id) }
            Button("Close Other Tabs") { model.closeOthers(than: tab.id) }
                .disabled(model.tabs.count < 2)
        }
    }

    private var tooltip: String {
        switch tab {
        case .table(let t): t.table.id
        case .script(let s): s.title
        }
    }
}

private struct CloseButton: View {
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: "xmark")
                .font(.system(size: 8, weight: .bold))
                .foregroundStyle(.secondary)
                .frame(width: 18, height: 18)
                .background(Circle().fill(.primary.opacity(hovering ? 0.12 : 0)))
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help("Close Tab")
    }
}

private struct NewTabButton: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Group {
            if #available(macOS 26.0, *) {
                button.buttonStyle(.glass).buttonBorderShape(.circle)
            } else {
                button.buttonStyle(.borderless)
            }
        }
        .disabled(model.selectedConnection == nil)
        .help("New SQL Script")
    }

    private var button: some View {
        Button { model.newScript() } label: {
            Image(systemName: "plus")
                .font(.system(size: 13, weight: .medium))
                .frame(width: 18, height: 18)
        }
    }
}
