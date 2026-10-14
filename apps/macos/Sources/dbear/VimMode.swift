import AppKit
import VimKit

/// Settings ▸ Editor ▸ Vim mode: the SQL editor gets normal/insert/visual modes and the result grids
/// move with h/j/k/l.
enum VimPreference {
    static let key = "vimMode"

    static var isOn: Bool { UserDefaults.standard.bool(forKey: key) }
}

/// Yanks go to the system clipboard; `p` pastes what was copied elsewhere since.
@MainActor
final class PasteboardClipboard: VimClipboard {
    var changeCount: Int { NSPasteboard.general.changeCount }

    func read() -> String? { NSPasteboard.general.string(forType: .string) }

    func write(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

extension VimRegisters {
    /// Shared by every editor, like Vim's registers across windows.
    static let app = VimRegisters(clipboard: PasteboardClipboard())
}

extension VimKey {
    /// The key for a key-down event, or `nil` for keys Vim doesn't use (⌘ shortcuts, function keys,
    /// modified arrows), which go to the text view as usual.
    init?(_ event: NSEvent) {
        let flags = event.modifierFlags.intersection([.command, .control, .option, .shift])
        if flags.contains(.command) { return nil }
        let plain = flags.isEmpty
        switch event.keyCode {
        case 53: self = .escape; return
        case 36, 76: if plain { self = .enter; return } else { return nil }
        case 51: if plain { self = .backspace; return } else { return nil }
        case 48: if plain { self = .tab; return } else { return nil }
        case 123: if plain { self = .left; return } else { return nil }
        case 124: if plain { self = .right; return } else { return nil }
        case 125: if plain { self = .down; return } else { return nil }
        case 126: if plain { self = .up; return } else { return nil }
        default: break
        }
        if flags.contains(.control) {
            guard flags == .control, let c = event.charactersIgnoringModifiers?.lowercased().first else { return nil }
            self = c == "[" ? .escape : .ctrl(c)
            return
        }
        guard let chars = event.characters, chars.count == 1, let c = chars.first,
              c.unicodeScalars.allSatisfy({ $0.value >= 32 && $0.value != 127 && !(0xF700...0xF8FF).contains($0.value) })
        else { return nil }
        self = .char(c)
    }
}

// MARK: - SQL editor

extension SQLTextView: VimBuffer {
    var text: NSString { string as NSString }

    var selection: NSRange { selectedRange() }

    func select(_ range: NSRange) {
        setSelectedRange(range)
        scrollRangeToVisible(range)
    }

    func replace(_ range: NSRange, with string: String) {
        guard shouldChangeText(in: range, replacementString: string) else { return }
        isApplyingVimEdit = true
        defer { isApplyingVimEdit = false }
        textStorage?.replaceCharacters(in: range, with: string)
        didChangeText()
    }

    func undoGroup(_ body: () -> Void) {
        guard let undoManager else { return body() }
        breakUndoCoalescing()
        undoManager.beginUndoGrouping()
        body()
        undoManager.endUndoGrouping()
    }

    func undo() {
        if undoManager?.canUndo == true { undoManager?.undo() }
    }

    func redo() {
        if undoManager?.canRedo == true { undoManager?.redo() }
    }

    var visibleLines: Int {
        guard let font else { return 20 }
        let lineHeight = font.ascender - font.descender + font.leading
        return max(Int(visibleRect.height / max(lineHeight, 1)), 1)
    }
}

/// The block cursor of normal and visual mode, over the character under the caret.
final class VimBlockCursor: NSView {
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 1
        isHidden = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    override func updateLayer() {
        layer?.backgroundColor = NSColor.controlAccentColor.withAlphaComponent(0.35).cgColor
    }

    override var wantsUpdateLayer: Bool { true }
}

/// "NORMAL", "INSERT"… and the keys typed so far, in the editor's bottom right corner.
final class VimModeLabel: NSTextField {
    convenience init() {
        self.init(labelWithString: "")
        font = .monospacedSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .medium)
        textColor = .secondaryLabelColor
        drawsBackground = true
        backgroundColor = .textBackgroundColor
        isHidden = true
    }

    func show(_ engine: VimEngine?) {
        guard let engine else {
            isHidden = true
            return
        }
        isHidden = false
        let mode = engine.mode == .insert ? "-- INSERT --" : engine.mode == .normal ? "NORMAL" : "-- \(engine.mode.label) --"
        let keys = engine.pendingKeys
        stringValue = keys.isEmpty ? mode : "\(keys)   \(mode)"
        pinToCorner()
    }

    /// Bottom right of the superview (an `NSScrollView`, which lays out its subviews itself, so no
    /// constraints): placed by hand, kept there by the autoresizing mask.
    private func pinToCorner() {
        guard let superview else { return }
        sizeToFit()
        let bounds = superview.bounds
        let x = bounds.maxX - frame.width - 10
        let y = superview.isFlipped ? bounds.maxY - frame.height - 6 : bounds.minY + 6
        setFrameOrigin(NSPoint(x: x, y: y))
        autoresizingMask = [.minXMargin, superview.isFlipped ? .minYMargin : .maxYMargin]
    }
}
