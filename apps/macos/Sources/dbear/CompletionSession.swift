import AppKit
import DBKit

/// SQL completion while typing in an `NSTextView` (the script editor). Decides when the popup
/// opens, refilters it, and inserts the accepted item.
/// The owner supplies `complete` and forwards text changes, selection changes and keys.
@MainActor
final class CompletionSession {
    let popup = CompletionPopup()
    weak var textView: NSTextView?
    /// Completions for a text with the caret at a UTF-16 offset; `nil` = nothing to offer (e.g. the
    /// schema catalog is still loading).
    var complete: ((_ text: String, _ location: Int) -> Completions?)?

    /// Range in the text the accepted item replaces; set right before the popup is shown.
    private var replaceRange: NSRange?
    private var pendingCompletion: DispatchWorkItem?
    /// Set by `textDidChange`, consumed by the selection change it triggers: that one is a side
    /// effect of typing, not the caret moving on its own, so it mustn't hide a debounced popup.
    private var selectionChangedByTyping = false
    /// Set while `accept(_:)` inserts the chosen item, so the resulting change doesn't reopen the popup.
    private var isAccepting = false

    private static let identifierChars = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "_"))
    /// Huge scripts skip automatic completion: cheap, but pointless on every keystroke there.
    private static let maxLength = 200_000

    init() {
        popup.onAccept = { [weak self] item in self?.accept(item) }
    }

    var isVisible: Bool { popup.isVisible }

    /// After the text changed. `inserted`: what was typed (`nil` or empty for a deletion).
    /// Completes right after `.`, debounced while typing a word, refilters on deletion (while the
    /// popup is open), and closes after anything else (space, `;`, `(`…).
    func textDidChange(inserted: String?) {
        selectionChangedByTyping = true
        guard !isAccepting else { return }
        guard let inserted, let last = inserted.last else {
            pendingCompletion?.cancel()
            if popup.isVisible { perform() }
            return
        }
        if last == "." {
            request(immediate: true)
        } else if String(last).rangeOfCharacter(from: Self.identifierChars) != nil {
            request(immediate: false)
        } else {
            dismiss()
        }
    }

    /// The caret moved on its own (click, arrow keys, running the script…): stop completing.
    func selectionDidChange() {
        defer { selectionChangedByTyping = false }
        guard !selectionChangedByTyping, popup.isVisible, let textView, let range = replaceRange else { return }
        if textView.selectedRange() != NSRange(location: NSMaxRange(range), length: 0) { popup.hide() }
    }

    /// ⌃Space: always shows the list, even if the word is already complete.
    func requestManual() {
        pendingCompletion?.cancel()
        perform(manual: true)
    }

    func moveSelection(by delta: Int) { popup.moveSelection(by: delta) }

    func acceptSelected() {
        if let item = popup.selectedItem { accept(item) } else { popup.hide() }
    }

    func dismiss() {
        pendingCompletion?.cancel()
        popup.hide()
    }

    /// Text-field style key handling (`control(_:textView:doCommandBy:)`): ↑↓ move, ⏎/⇥ accept,
    /// ⎋ closes, other caret moves (←, →, ⌘←…) close and still move. Deleting goes through
    /// untouched: the change refilters the list. Returns whether the command was used up.
    func handle(command selector: Selector) -> Bool {
        guard popup.isVisible else { return false }
        switch selector {
        case #selector(NSResponder.moveDown(_:)): moveSelection(by: 1)
        case #selector(NSResponder.moveUp(_:)): moveSelection(by: -1)
        case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertTab(_:)): acceptSelected()
        case #selector(NSResponder.cancelOperation(_:)): dismiss()
        default:
            if NSStringFromSelector(selector).hasPrefix("move") { dismiss() }
            return false
        }
        return true
    }

    private func request(immediate: Bool) {
        pendingCompletion?.cancel()
        guard immediate else {
            let work = DispatchWorkItem { [weak self] in self?.perform() }
            pendingCompletion = work
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.12, execute: work)
            return
        }
        perform()
    }

    private func perform(manual: Bool = false) {
        guard !isAccepting, let textView, let window = textView.window else {
            popup.hide()
            return
        }
        let source = textView.string
        guard source.utf16.count < Self.maxLength else { return }
        let location = textView.selectedRange().location
        guard let result = complete?(source, location) else {
            popup.hide()
            return
        }
        guard textView.selectedRange().location == location else { return } // caret moved meanwhile
        // Nothing left to complete: the word typed already is the only suggestion.
        let typed = (source as NSString).substring(with: result.range)
        let onlyExactMatches = result.items.allSatisfy { $0.label.caseInsensitiveCompare(typed) == .orderedSame }
        guard !result.items.isEmpty, manual || !onlyExactMatches else {
            popup.hide()
            return
        }
        replaceRange = result.range
        var actual = NSRange()
        let screenRect = textView.firstRect(forCharacterRange: result.range, actualRange: &actual)
        popup.show(items: result.items, below: screenRect, in: window)
    }

    private func accept(_ item: CompletionItem) {
        popup.hide()
        guard let textView, let range = replaceRange, NSMaxRange(range) <= (textView.string as NSString).length else { return }
        pendingCompletion?.cancel()
        isAccepting = true
        textView.insertText(item.insertText, replacementRange: range)
        // The change notification isn't guaranteed to arrive inside `insertText`; keep
        // suppressing auto-completion until this run loop turn is over.
        DispatchQueue.main.async { [weak self] in
            self?.pendingCompletion?.cancel()
            self?.isAccepting = false
        }
    }
}
