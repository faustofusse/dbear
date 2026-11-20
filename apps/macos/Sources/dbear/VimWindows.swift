import AppKit
import SwiftUI
import VimKit

/// Vim mode across the window: ⌃W commands move between panes (the sidebars, the SQL editor, result
/// grids, the inspector), `gt`/`gT` switch tabs, and j/k/h/l move in the sidebar lists.
///
/// ⌃W works wherever Vim would take it as a command: not while typing (the editor's insert mode, a
/// text field, a cell being edited).
@MainActor
final class VimWindowNavigator {
    static let shared = VimWindowNavigator()

    private weak var model: AppModel?
    private var monitor: Any?
    /// ⌃W was typed; the next key is the window command.
    private var pending = false
    /// The pane with focus the last time we looked, and the one before it (`⌃W p`).
    private weak var focused: NSView?
    private weak var previous: NSView?

    func install(model: AppModel) {
        self.model = model
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .leftMouseDown]) { [weak self] event in
            self?.handle(event) ?? event
        }
    }

    // MARK: Keys

    private func handle(_ event: NSEvent) -> NSEvent? {
        guard event.type == .keyDown else {
            pending = false
            return event
        }
        guard VimPreference.isOn, let window = event.window, window.attachedSheet == nil else {
            pending = false
            return event
        }
        if pending {
            pending = false
            if let key = VimKey(event), let command = VimWindowCommand(key) {
                run(command, in: window)
            }
            return nil  // Vim drops a key that isn't a window command too
        }
        let responder = window.firstResponder
        if VimKey(event) == .ctrl("w"), takesCommands(responder) {
            pending = true
            return nil
        }
        if let list = responder as? NSTableView, !(list is GridTableView), let arrow = Self.listArrow(for: event) {
            return arrow
        }
        return event
    }

    /// Whether keys typed in `responder` are commands rather than text.
    private func takesCommands(_ responder: NSResponder?) -> Bool {
        if let editor = responder as? SQLTextView {
            guard let vim = editor.vim else { return false }
            return vim.mode != .insert && !editor.hasMarkedText()
        }
        if let text = responder as? NSTextView { return !text.isFieldEditor && !text.isEditable }
        return true
    }

    /// j/k/h/l in a sidebar list: the arrow keys (SwiftUI lists move their selection with them).
    private static func listArrow(for event: NSEvent) -> NSEvent? {
        guard event.modifierFlags.intersection([.command, .control, .option, .shift]).isEmpty,
              let c = event.characters, c.count == 1 else { return nil }
        let arrows: [String: (UInt16, Int)] = [
            "j": (125, NSDownArrowFunctionKey), "k": (126, NSUpArrowFunctionKey),
            "h": (123, NSLeftArrowFunctionKey), "l": (124, NSRightArrowFunctionKey),
        ]
        guard let (keyCode, function) = arrows[c], let scalar = UnicodeScalar(function) else { return nil }
        let chars = String(Character(scalar))
        return NSEvent.keyEvent(
            with: .keyDown, location: event.locationInWindow, modifierFlags: [.numericPad, .function],
            timestamp: event.timestamp, windowNumber: event.windowNumber, context: nil,
            characters: chars, charactersIgnoringModifiers: chars, isARepeat: event.isARepeat, keyCode: keyCode)
    }

    // MARK: Commands

    private func run(_ command: VimWindowCommand, in window: NSWindow) {
        let panes = Self.panes(in: window)
        let current = noteFocus(panes: panes, in: window)
        switch command {
        case .last:
            if let previous, panes.contains(where: { $0 === previous }) { focus(previous, in: window) }
        case .only:
            model?.sidebarVisibility = .detailOnly
            model?.showsInspector = false
        case .close:
            if let model { AppCommand.closeTab.perform(model) }
        case .new:
            if let model, AppCommand.newScript.isEnabled(model) { model.newScript() }
        default:
            let frames = panes.map { Self.frame(of: $0) }
            let index = current.flatMap { c in panes.firstIndex { $0 === c } }
            if let target = VimPanes.target(command, from: index, in: frames) { focus(panes[target], in: window) }
        }
    }

    /// The pane holding the focus now; keeps `focused`/`previous` up to date when it moved
    /// (e.g. with a click) since we last looked.
    @discardableResult
    private func noteFocus(panes: [NSView], in window: NSWindow) -> NSView? {
        let current = Self.pane(holding: window.firstResponder, in: panes)
        if let current, current !== focused {
            previous = focused
            focused = current
        }
        return current
    }

    private func focus(_ pane: NSView, in window: NSWindow) {
        guard pane !== focused || window.firstResponder !== pane else { return }
        if pane !== focused {
            previous = focused
            focused = pane
        }
        window.makeFirstResponder(pane)
        if let table = pane as? NSTableView, table.selectedRow < 0, table.numberOfRows > 0, table is GridTableView {
            table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        }
    }

    // MARK: Tabs

    /// `gt` / `gT` from the editor or a grid. Focus moves to the new tab's pane in the same place.
    func switchTab(forward: Bool, count: Int?, from view: NSView) {
        guard let model, !model.tabs.isEmpty else { return }
        let current = model.tabs.firstIndex { $0.id == model.activeTabID } ?? 0
        let n = model.tabs.count
        let index: Int
        if forward {
            index = count.map { min(max($0, 1), n) - 1 } ?? (current + 1) % n
        } else {
            index = ((current - (count ?? 1)) % n + n) % n
        }
        guard index != current, let window = view.window else { return }
        let spot = Self.frame(of: view)
        model.activate(model.tabs[index].id)
        // After SwiftUI puts the new tab's views in.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak self] in
            guard let self, Self.pane(holding: window.firstResponder, in: Self.panes(in: window)) == nil else { return }
            let panes = Self.panes(in: window)
            let center = CGPoint(x: spot.midX, y: spot.midY)
            let nearest = panes.min { a, b in
                Self.distance(from: center, to: Self.frame(of: a)) < Self.distance(from: center, to: Self.frame(of: b))
            }
            if let nearest { focus(nearest, in: window) }
        }
    }

    private static func distance(from point: CGPoint, to rect: CGRect) -> CGFloat {
        let dx = max(rect.minX - point.x, 0, point.x - rect.maxX)
        let dy = max(rect.minY - point.y, 0, point.y - rect.maxY)
        return dx * dx + dy * dy
    }

    // MARK: Panes

    /// The views ⌃W moves between, in the window: lists and grids, the SQL editor, read-only and
    /// inspector text views. Hidden and collapsed ones are left out.
    static func panes(in window: NSWindow) -> [NSView] {
        guard let content = window.contentView else { return [] }
        var panes: [NSView] = []
        func visit(_ view: NSView) {
            if view.isHidden { return }
            if isPane(view) {
                let frame = (view.enclosingScrollView ?? view).visibleRect
                if frame.width > 20, frame.height > 20, view.acceptsFirstResponder { panes.append(view) }
                return
            }
            view.subviews.forEach(visit)
        }
        visit(content)
        return panes
    }

    private static func isPane(_ view: NSView) -> Bool {
        if view is NSTableView { return true }
        if let text = view as? NSTextView { return !text.isFieldEditor && text.enclosingScrollView != nil }
        return false
    }

    private static func pane(holding responder: NSResponder?, in panes: [NSView]) -> NSView? {
        guard let view = responder as? NSView else { return nil }
        return panes.first { view === $0 || view.isDescendant(of: $0) }
    }

    /// Where the pane shows on screen, in window coordinates with the origin at the top left.
    private static func frame(of view: NSView) -> CGRect {
        let shown = view.enclosingScrollView ?? view
        let rect = shown.convert(shown.visibleRect, to: nil)
        let height = shown.window?.contentView?.bounds.height ?? 0
        return CGRect(x: rect.minX, y: height - rect.maxY, width: rect.width, height: rect.height)
    }
}
