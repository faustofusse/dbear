import AppKit
import DBKit
import SwiftUI

/// Plain-text SQL editor (NSTextView) with tree-sitter highlighting from the Rust core.
struct SQLEditor: NSViewRepresentable {
    @Binding var text: String
    /// Make the editor first responder with the caret at the end once it's in a window.
    var focusOnAppear = false
    var fontSize: CGFloat = NSFont.systemFontSize
    /// ⌘= (the unshifted ⌘+ on US layouts) while the editor has focus; the menu handles ⌘+ and ⌘-.
    var onZoomIn: () -> Void = {}
    /// Selection to restore when the editor is created (e.g. switching back to the tab).
    var initialSelection: [NSRange] = []
    /// Selected UTF-16 ranges whenever the selection or caret moves.
    var onSelectionChange: ([NSRange]) -> Void = { _ in }
    /// Schema catalog for completion; `nil` while it's loading (⌃Space and autocomplete do nothing).
    var completionCatalog: CompletionCatalog?
    var databaseKind: DatabaseKind = .postgres

    func makeCoordinator() -> Coordinator {
        Coordinator(text: $text, theme: SQLTheme(fontSize: fontSize), onSelectionChange: onSelectionChange)
    }

    func makeNSView(context: Context) -> NSScrollView {
        let scrollView = SQLTextView.scrollableTextView()
        scrollView.drawsBackground = false
        scrollView.hasHorizontalScroller = false

        let textView = scrollView.documentView as! SQLTextView
        textView.onZoomIn = onZoomIn
        textView.completionHandler = context.coordinator
        textView.delegate = context.coordinator
        textView.drawsBackground = false
        textView.isRichText = false
        textView.importsGraphics = false
        textView.allowsUndo = true
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true
        textView.smartInsertDeleteEnabled = false
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.textContainerInset = NSSize(width: 10, height: 8)
        // Only tint the background, so selected SQL keeps its syntax colors (like Xcode).
        textView.selectedTextAttributes = [.backgroundColor: NSColor.selectedTextBackgroundColor]
        let theme = context.coordinator.theme
        textView.font = theme.font
        textView.typingAttributes = theme.baseAttributes

        textView.string = text
        context.coordinator.textView = textView
        context.coordinator.completion.textView = textView
        context.coordinator.catalog = completionCatalog
        context.coordinator.databaseKind = databaseKind
        context.coordinator.highlight()
        let length = (text as NSString).length
        let restored = initialSelection.filter { NSMaxRange($0) <= length }
        if !focusOnAppear, !restored.isEmpty {
            textView.selectedRanges = restored.map { NSValue(range: $0) }
        }

        if focusOnAppear {
            // Not in a window yet; wait a run loop turn.
            DispatchQueue.main.async { [weak textView] in
                guard let textView, let window = textView.window else { return }
                window.makeFirstResponder(textView)
                textView.setSelectedRange(NSRange(location: (textView.string as NSString).length, length: 0))
            }
        }
        return scrollView
    }

    func updateNSView(_ scrollView: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        coordinator.text = $text
        coordinator.onSelectionChange = onSelectionChange
        coordinator.catalog = completionCatalog
        coordinator.databaseKind = databaseKind
        guard let textView = coordinator.textView as? SQLTextView else { return }
        textView.onZoomIn = onZoomIn
        if coordinator.theme.fontSize != fontSize {
            coordinator.theme = SQLTheme(fontSize: fontSize)
            textView.font = coordinator.theme.font
            textView.typingAttributes = coordinator.theme.baseAttributes
            coordinator.highlight()
        }
        guard textView.string != text else { return }
        // External change (not typed here): replace and keep the caret in range.
        let caret = min(textView.selectedRange().location, (text as NSString).length)
        textView.string = text
        textView.setSelectedRange(NSRange(location: caret, length: 0))
        context.coordinator.highlight()
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var text: Binding<String>
        var theme: SQLTheme
        var onSelectionChange: ([NSRange]) -> Void
        weak var textView: NSTextView?
        private var generation = 0

        // MARK: Completion
        var catalog: CompletionCatalog?
        var databaseKind: DatabaseKind = .postgres
        /// This editor supplies the whole script to the completion session.
        let completion = CompletionSession()
        /// Text just inserted (or `nil`/empty for a deletion), captured before the change lands.
        private var lastInsertedText: String?

        init(text: Binding<String>, theme: SQLTheme, onSelectionChange: @escaping ([NSRange]) -> Void) {
            self.text = text
            self.theme = theme
            self.onSelectionChange = onSelectionChange
            super.init()
            completion.complete = { [weak self] text, location in
                guard let self, let catalog = self.catalog else { return nil }
                return catalog.complete(text: text, location: location, kind: self.databaseKind)
            }
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard let textView else { return }
            onSelectionChange(textView.selectedRanges.map(\.rangeValue))
            completion.selectionDidChange()
        }

        func textView(_ textView: NSTextView, shouldChangeTextIn affectedCharRange: NSRange, replacementString: String?) -> Bool {
            lastInsertedText = replacementString
            return true
        }

        func textDidChange(_ notification: Notification) {
            guard let textView else { return }
            text.wrappedValue = textView.string
            highlight()
            completion.textDidChange(inserted: lastInsertedText)
        }

        /// Small scripts are highlighted synchronously (no flash of unstyled text);
        /// huge ones off the main thread, applied only if the text hasn't changed meanwhile.
        func highlight() {
            guard let textView else { return }
            let source = textView.string
            generation += 1
            if source.utf16.count < 50_000 {
                apply(SQLSyntax.highlight(source), to: textView)
                return
            }
            let generation = generation
            Task.detached(priority: .userInitiated) {
                let spans = SQLSyntax.highlight(source)
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

extension SQLEditor.Coordinator: CompletionKeyHandling {
    var isCompletionVisible: Bool { completion.isVisible }
    func moveCompletionSelection(by delta: Int) { completion.moveSelection(by: delta) }
    func dismissCompletion() { completion.dismiss() }
    func acceptCompletion() { completion.acceptSelected() }
    func requestManualCompletion() { completion.requestManual() }
}

/// Lets `SQLTextView` forward keys to the completion popup without depending on `SQLEditor` itself.
@MainActor
protocol CompletionKeyHandling: AnyObject {
    var isCompletionVisible: Bool { get }
    func moveCompletionSelection(by delta: Int)
    func acceptCompletion()
    func dismissCompletion()
    /// \u2303Space: complete now, even if the word is already complete.
    func requestManualCompletion()
}

final class SQLTextView: NSTextView {
    var onZoomIn: () -> Void = {}
    weak var completionHandler: CompletionKeyHandling?

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        if flags == .command, event.charactersIgnoringModifiers == "=" {
            onZoomIn()
            return true
        }
        return super.performKeyEquivalent(with: event)
    }

    override func keyDown(with event: NSEvent) {
        if let handler = completionHandler, handler.isCompletionVisible {
            switch event.keyCode {
            case 125: handler.moveCompletionSelection(by: 1); return // Down arrow
            case 126: handler.moveCompletionSelection(by: -1); return // Up arrow
            case 36, 76, 48: handler.acceptCompletion(); return // Return, Enter, Tab
            case 53: handler.dismissCompletion(); return // Escape
            default: break
            }
        }
        // \u2303Space: works whether or not the popup is already open.
        if event.keyCode == 49, event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .control {
            completionHandler?.requestManualCompletion()
            return
        }
        super.keyDown(with: event)
    }
}

/// Xcode-like palette (Default Light / Default Dark), resolved per appearance at draw time.
@MainActor
struct SQLTheme {
    let fontSize: CGFloat
    let font: NSFont
    let baseAttributes: [NSAttributedString.Key: Any]
    private let kinds: [SyntaxKind: [NSAttributedString.Key: Any]]

    /// The inspector's JSON theme: like SQL, plus a color for object keys (`.field`).
    static func json(fontSize: CGFloat) -> SQLTheme {
        SQLTheme(fontSize: fontSize, fieldColor: dynamic(light: 0x0B4F79, dark: 0x5DD8FF))
    }

    init(fontSize: CGFloat) {
        self.init(fontSize: fontSize, fieldColor: nil)
    }

    private init(fontSize: CGFloat, fieldColor: NSColor?) {
        self.fontSize = fontSize
        font = NSFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        let keywordFont = NSFont.monospacedSystemFont(ofSize: fontSize, weight: .semibold)
        baseAttributes = [.font: font, .foregroundColor: NSColor.textColor]
        var kinds: [SyntaxKind: [NSAttributedString.Key: Any]] = [:]
        for kind in SyntaxKind.allCases {
            var attrs: [NSAttributedString.Key: Any] = [:]
            if let color = kind == .field ? fieldColor : Self.colors[kind] { attrs[.foregroundColor] = color }
            if kind == .keyword || kind == .constant { attrs[.font] = keywordFont }
            kinds[kind] = attrs
        }
        self.kinds = kinds
    }

    func attributes(for kind: SyntaxKind) -> [NSAttributedString.Key: Any] {
        kinds[kind] ?? [:]
    }

    private static let colors: [SyntaxKind: NSColor] = {
        var result: [SyntaxKind: NSColor] = [:]
        for kind in SyntaxKind.allCases { result[kind] = color(for: kind) }
        return result
    }()

    private static func color(for kind: SyntaxKind) -> NSColor? {
        switch kind {
        case .keyword, .constant: dynamic(light: 0x9B2393, dark: 0xFC5FA3)
        case .type: dynamic(light: 0x0B4F79, dark: 0x5DD8FF)
        case .object: dynamic(light: 0x1C464A, dark: 0x9EF1DD)
        case .function: dynamic(light: 0x326D74, dark: 0x67B7A4)
        case .string: dynamic(light: 0xC41A16, dark: 0xFC6A5D)
        case .number: dynamic(light: 0x1C00CF, dark: 0xD0BF69)
        case .comment: dynamic(light: 0x5D6C79, dark: 0x7F8C98)
        case .parameter: dynamic(light: 0x643820, dark: 0xFD8F3F)
        case .variable: dynamic(light: 0x3E8087, dark: 0x67B7A4)
        case .field, .operator, .punctuation: nil
        }
    }

    private static func dynamic(light: UInt32, dark: UInt32) -> NSColor {
        NSColor(name: nil) { appearance in
            let isDark = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            return rgb(isDark ? dark : light)
        }
    }

    private static func rgb(_ hex: UInt32) -> NSColor {
        NSColor(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
                green: CGFloat((hex >> 8) & 0xFF) / 255,
                blue: CGFloat(hex & 0xFF) / 255,
                alpha: 1)
    }
}
