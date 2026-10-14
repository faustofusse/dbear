import Foundation

public enum VimMode: Equatable, Sendable {
    case normal, insert, visual, visualLine

    public var label: String {
        switch self {
        case .normal: "NORMAL"
        case .insert: "INSERT"
        case .visual: "VISUAL"
        case .visualLine: "VISUAL LINE"
        }
    }
}

/// A key as Vim sees it: `char` has Shift already applied, `ctrl` is a lowercase letter.
public enum VimKey: Hashable, Sendable {
    case char(Character)
    case ctrl(Character)
    case escape, enter, backspace, tab, left, right, up, down
}

/// The text view the engine drives.
@MainActor
public protocol VimBuffer: AnyObject {
    /// A snapshot of the whole text.
    var text: NSString { get }
    var selection: NSRange { get }
    /// Selects `range` (a caret when empty) and scrolls it into view.
    func select(_ range: NSRange)
    /// Replaces `range` as an undoable edit.
    func replace(_ range: NSRange, with string: String)
    /// Runs `body`'s edits as one undo step.
    func undoGroup(_ body: () -> Void)
    func undo()
    func redo()
    /// Lines the view shows at once (⌃D/⌃U move half of them).
    var visibleLines: Int { get }
}

@MainActor
public protocol VimClipboard: AnyObject {
    /// Changes whenever anyone writes to the clipboard.
    var changeCount: Int { get }
    func read() -> String?
    func write(_ text: String)
}

/// The unnamed register, shared by every editor. Yanks also go to the clipboard, and `p` pastes
/// what was copied elsewhere since the last yank.
@MainActor
public final class VimRegisters {
    public struct Entry: Equatable, Sendable {
        public var text: String
        public var linewise: Bool

        public init(text: String, linewise: Bool) {
            self.text = text
            self.linewise = linewise
        }
    }

    public var clipboard: VimClipboard?
    private var unnamed: Entry?
    private var seenChangeCount: Int?

    public init(clipboard: VimClipboard? = nil) {
        self.clipboard = clipboard
    }

    func store(_ entry: Entry, yank: Bool) {
        unnamed = entry
        guard yank, let clipboard else { return }
        clipboard.write(entry.text)
        seenChangeCount = clipboard.changeCount
    }

    func paste() -> Entry? {
        if let clipboard, clipboard.changeCount != seenChangeCount {
            seenChangeCount = clipboard.changeCount
            if let text = clipboard.read(), !text.isEmpty {
                unnamed = Entry(text: text, linewise: text.hasSuffix("\n"))
            }
        }
        return unnamed
    }
}

/// Vim's normal, insert and visual modes over a `VimBuffer`. Insert mode leaves keys to the text
/// view (only ⎋ is used); the other modes use every key they get.
@MainActor
public final class VimEngine {
    public private(set) var mode: VimMode = .normal
    /// Mode or pending keys changed.
    public var onChange: (() -> Void)?

    private weak var buffer: VimBuffer?
    private let registers: VimRegisters
    private var pending: [VimKey] = []
    /// Visual mode: where it started, and the end that moves (a character position each).
    private var anchor = 0
    private var cursor = 0
    /// The selection visual mode last set, to notice the mouse changing it.
    private var visualSelection: NSRange?
    /// Column `j`/`k` aim for (`Int.max` after `$`).
    private var desiredColumn: Int?
    private var lastFind: (char: Character, forward: Bool, till: Bool)?

    private struct Change {
        var keys: [VimKey]
        var count: Int?
        var inserted: String?
    }
    private var lastChange: Change?
    /// The change whose insert session is running (finished by ⎋).
    private var recording: Change?
    private var insertStart = 0
    private var insertLength = 0
    private var replaying = false

    public init(buffer: VimBuffer, registers: VimRegisters) {
        self.buffer = buffer
        self.registers = registers
    }

    /// Typed keys of a command that isn't complete yet (`2d`, `ci`…).
    public var pendingKeys: String {
        pending.map { key in
            switch key {
            case .char(let c): String(c)
            case .ctrl(let c): "^" + c.uppercased()
            case .escape: "<Esc>"
            case .enter: "<CR>"
            case .backspace: "<BS>"
            case .tab: "<Tab>"
            case .left: "<Left>"
            case .right: "<Right>"
            case .up: "<Up>"
            case .down: "<Down>"
            }
        }.joined()
    }

    /// Where a caret put at `location` (e.g. by a click) sits in normal mode: on a character, not
    /// past the end of its line.
    public static func normalCaret(_ location: Int, in text: NSString) -> Int {
        VimText(text).clampNormal(location)
    }

    /// Back to normal mode with nothing pending (e.g. when Vim mode is turned on).
    public func reset() {
        pending = []
        recording = nil
        guard let buffer else { return }
        mode = .normal
        let caret = buffer.selection.location
        buffer.select(NSRange(location: VimText(buffer.text).clampNormal(caret), length: 0))
        changed()
    }

    /// Returns whether the key was used; in insert mode only ⎋ is.
    @discardableResult
    public func handle(_ key: VimKey) -> Bool {
        guard let buffer else { return false }
        switch mode {
        case .insert:
            guard key == .escape else { return false }
            finishInsert()
        case .normal:
            let selection = buffer.selection
            if pending.isEmpty, selection.length > 0 {
                // A selection made with the mouse (or ⌘A): continue in visual mode.
                let t = text
                mode = .visual
                anchor = selection.location
                cursor = t.prev(NSMaxRange(selection))
                visualSelection = selection
                handleVisual(key)
            } else {
                handleNormal(key)
            }
        case .visual, .visualLine:
            handleVisual(key)
        }
        changed()
        return true
    }

    private var text: VimText { VimText(buffer?.text ?? "") }

    private func changed() { onChange?() }

    // MARK: Parsing

    private enum Motion: Equatable {
        case left, right, up, down, nextLine, prevLine
        case lineStart, firstNonBlank, lineEnd
        case wordForward(big: Bool), wordBackward(big: Bool), wordEnd(big: Bool)
        case gotoLine(last: Bool)
        case paragraph(forward: Bool)
        case matchBracket
        case find(Character, forward: Bool, till: Bool)
        case repeatFind(reverse: Bool)
        case halfPage(down: Bool)
    }

    private enum Operator: Character {
        case delete = "d", change = "c", yank = "y"
    }

    private enum Target {
        case motion(Motion)
        case object(inner: Bool, kind: Character)
        case lines
    }

    private enum Command {
        case move(Motion)
        case operate(Operator, Target)
        case insert(Character)
        case replaceChar(Character)
        case join
        case put(after: Bool)
        case undo, redo, repeatChange
        case visual(line: Bool)
        case toggleCase
    }

    private enum Parsed<T> {
        case incomplete, invalid
        case done(T, count: Int?)
    }

    private static let objectKinds: Set<Character> = ["w", "W", "\"", "'", "`", "(", ")", "b", "[", "]", "{", "}", "B", "<", ">"]

    private static func parseCount(_ keys: [VimKey], _ i: inout Int) -> Int? {
        var count: Int?
        while i < keys.count, case .char(let c) = keys[i], let digit = c.wholeNumberValue, c.isASCII {
            if count == nil, digit == 0 { break }
            count = min((count ?? 0) * 10 + digit, 99_999)
            i += 1
        }
        return count
    }

    private static func multiply(_ a: Int?, _ b: Int?) -> Int? {
        switch (a, b) {
        case (nil, nil): nil
        case let (a?, nil): a
        case let (nil, b?): b
        case let (a?, b?): min(a * b, 99_999)
        }
    }

    private enum MotionParse {
        case incomplete, invalid
        case motion(Motion)
    }

    private static func parseMotion(_ keys: [VimKey], _ i: inout Int) -> MotionParse {
        guard i < keys.count else { return .incomplete }
        let key = keys[i]
        i += 1
        switch key {
        case .left, .backspace, .char("h"): return .motion(.left)
        case .right, .char("l"), .char(" "): return .motion(.right)
        case .down, .char("j"), .ctrl("n"), .ctrl("j"): return .motion(.down)
        case .up, .char("k"), .ctrl("p"): return .motion(.up)
        case .enter, .char("+"): return .motion(.nextLine)
        case .char("-"): return .motion(.prevLine)
        case .char("0"): return .motion(.lineStart)
        case .char("^"): return .motion(.firstNonBlank)
        case .char("$"): return .motion(.lineEnd)
        case .char("w"): return .motion(.wordForward(big: false))
        case .char("W"): return .motion(.wordForward(big: true))
        case .char("b"): return .motion(.wordBackward(big: false))
        case .char("B"): return .motion(.wordBackward(big: true))
        case .char("e"): return .motion(.wordEnd(big: false))
        case .char("E"): return .motion(.wordEnd(big: true))
        case .char("G"): return .motion(.gotoLine(last: true))
        case .char("{"): return .motion(.paragraph(forward: false))
        case .char("}"): return .motion(.paragraph(forward: true))
        case .char("%"): return .motion(.matchBracket)
        case .char(";"): return .motion(.repeatFind(reverse: false))
        case .char(","): return .motion(.repeatFind(reverse: true))
        case .ctrl("d"): return .motion(.halfPage(down: true))
        case .ctrl("u"): return .motion(.halfPage(down: false))
        case .char("g"):
            guard i < keys.count else { return .incomplete }
            guard keys[i] == .char("g") else { return .invalid }
            i += 1
            return .motion(.gotoLine(last: false))
        case .char(let c) where "fFtT".contains(c):
            guard i < keys.count else { return .incomplete }
            guard case .char(let target) = keys[i] else { return .invalid }
            i += 1
            return .motion(.find(target, forward: c == "f" || c == "t", till: c == "t" || c == "T"))
        default:
            return .invalid
        }
    }

    private enum ObjectParse {
        case incomplete, invalid
        case object(inner: Bool, kind: Character)
    }

    /// `i`/`a` followed by an object kind, at `keys[i]`; `nil` when `keys[i]` isn't `i`/`a`.
    private static func parseObject(_ keys: [VimKey], _ i: inout Int) -> ObjectParse? {
        guard i < keys.count, keys[i] == .char("i") || keys[i] == .char("a") else { return nil }
        let inner = keys[i] == .char("i")
        i += 1
        guard i < keys.count else { return .incomplete }
        guard case .char(let kind) = keys[i], objectKinds.contains(kind) else { return .invalid }
        i += 1
        return .object(inner: inner, kind: kind)
    }

    private static func parseNormal(_ keys: [VimKey]) -> Parsed<Command> {
        var i = 0
        let count = parseCount(keys, &i)
        guard i < keys.count else { return .incomplete }
        let key = keys[i]
        i += 1
        func done(_ command: Command, _ count: Int?) -> Parsed<Command> {
            i == keys.count ? .done(command, count: count) : .invalid
        }
        switch key {
        case .char(let c) where c == "d" || c == "c" || c == "y":
            let op = Operator(rawValue: c)!
            let total = multiply(count, parseCount(keys, &i))
            guard i < keys.count else { return .incomplete }
            if keys[i] == .char(c) {
                i += 1
                return done(.operate(op, .lines), total)
            }
            if let object = parseObject(keys, &i) {
                switch object {
                case .incomplete: return .incomplete
                case .invalid: return .invalid
                case .object(let inner, let kind): return done(.operate(op, .object(inner: inner, kind: kind)), total)
                }
            }
            switch parseMotion(keys, &i) {
            case .incomplete: return .incomplete
            case .invalid: return .invalid
            case .motion(.halfPage): return .invalid
            case .motion(let motion): return done(.operate(op, .motion(motion)), total)
            }
        case .char("x"): return done(.operate(.delete, .motion(.right)), count)
        case .char("X"): return done(.operate(.delete, .motion(.left)), count)
        case .char("s"): return done(.operate(.change, .motion(.right)), count)
        case .char("S"): return done(.operate(.change, .lines), count)
        case .char("D"): return done(.operate(.delete, .motion(.lineEnd)), count)
        case .char("C"): return done(.operate(.change, .motion(.lineEnd)), count)
        case .char("Y"): return done(.operate(.yank, .lines), count)
        case .char(let c) where "iaIAoO".contains(c): return done(.insert(c), count)
        case .char("r"):
            guard i < keys.count else { return .incomplete }
            guard case .char(let c) = keys[i] else { return .invalid }
            i += 1
            return done(.replaceChar(c), count)
        case .char("J"): return done(.join, count)
        case .char("p"): return done(.put(after: true), count)
        case .char("P"): return done(.put(after: false), count)
        case .char("u"): return done(.undo, count)
        case .ctrl("r"): return done(.redo, count)
        case .char("."): return done(.repeatChange, count)
        case .char("v"): return done(.visual(line: false), count)
        case .char("V"): return done(.visual(line: true), count)
        case .char("~"): return done(.toggleCase, count)
        default:
            i -= 1
            switch parseMotion(keys, &i) {
            case .incomplete: return .incomplete
            case .invalid: return .invalid
            case .motion(let motion): return done(.move(motion), count)
            }
        }
    }

    // MARK: Normal mode

    private func handleNormal(_ key: VimKey) {
        guard let buffer else { return }
        if pending.isEmpty {
            cursor = text.clampNormal(buffer.selection.location)
        }
        pending.append(key)
        switch Self.parseNormal(pending) {
        case .incomplete:
            return
        case .invalid:
            pending = []
        case .done(let command, let count):
            let keys = pending
            pending = []
            execute(command, count: count, keys: keys)
        }
    }

    private func execute(_ command: Command, count: Int?, keys: [VimKey]) {
        guard let buffer else { return }
        let isChange: Bool
        switch command {
        case .operate(let op, _): isChange = op != .yank
        case .insert, .replaceChar, .join, .put, .toggleCase: isChange = true
        default: isChange = false
        }
        switch command {
        case .move(let motion):
            move(motion, count: count)
            return
        // Undo can't run inside an undo group.
        case .undo:
            undo(count: count ?? 1, redo: false)
            return
        case .redo:
            undo(count: count ?? 1, redo: true)
            return
        default: break
        }
        desiredColumn = nil
        buffer.undoGroup {
            switch command {
            case .move, .undo, .redo: break
            case .operate(let op, let target): operate(op, target, count: count)
            case .insert(let kind): insert(kind)
            case .replaceChar(let c): replaceChars(with: c, count: count ?? 1)
            case .join: join(lines: max(count ?? 2, 2))
            case .put(let after): put(after: after, count: count ?? 1)
            case .repeatChange: repeatChange(count: count)
            case .visual(let line):
                mode = line ? .visualLine : .visual
                anchor = cursor
                updateVisualSelection()
            case .toggleCase: toggleCase(count: count ?? 1)
            }
        }
        guard isChange, !replaying else { return }
        // `.` repeats the command without its count, which `3.` can replace.
        var start = 0
        _ = Self.parseCount(keys, &start)
        var leading = 0
        let change = Change(keys: Array(keys[start...]), count: Self.parseCount(keys, &leading))
        if mode == .insert { recording = change } else { lastChange = change }
    }

    private func setCursor(_ p: Int) {
        let t = text
        cursor = t.clampNormal(p)
        buffer?.select(NSRange(location: cursor, length: 0))
    }

    private func move(_ motion: Motion, count: Int?) {
        guard let reach = evaluate(motion, count: count, from: cursor) else { return }
        updateDesiredColumn(for: motion)
        setCursor(reach.target)
    }

    private func updateDesiredColumn(for motion: Motion) {
        switch motion {
        case .up, .down, .halfPage: break
        case .lineEnd: desiredColumn = Int.max
        default: desiredColumn = nil
        }
    }

    // MARK: Motions

    private enum RangeKind { case exclusive, inclusive, linewise }

    private struct Reach {
        var target: Int
        var kind: RangeKind
    }

    private func evaluate(_ motion: Motion, count: Int?, from p: Int) -> Reach? {
        let t = text
        let n = max(count ?? 1, 1)
        func vertical(down: Bool, lines: Int, firstNonBlank: Bool) -> Reach? {
            var line = t.lineStart(p), moved = 0
            for _ in 0..<lines {
                guard let next = down ? t.nextLineStart(line) : t.prevLineStart(line) else { break }
                line = next
                moved += 1
            }
            guard moved > 0 else { return nil }
            if firstNonBlank { return Reach(target: t.firstNonBlank(line), kind: .linewise) }
            if desiredColumn == nil { desiredColumn = t.column(p) }
            return Reach(target: t.position(lineStart: line, column: desiredColumn!), kind: .linewise)
        }
        switch motion {
        case .left:
            let start = t.lineStart(p)
            var q = p
            for _ in 0..<n where q > start { q = t.prev(q) }
            return q == p ? nil : Reach(target: q, kind: .exclusive)
        case .right:
            let end = t.contentEnd(p)
            var q = p
            for _ in 0..<n where q < end { q = t.next(q) }
            return q == p ? nil : Reach(target: q, kind: .exclusive)
        case .down: return vertical(down: true, lines: n, firstNonBlank: false)
        case .up: return vertical(down: false, lines: n, firstNonBlank: false)
        case .nextLine: return vertical(down: true, lines: n, firstNonBlank: true)
        case .prevLine: return vertical(down: false, lines: n, firstNonBlank: true)
        case .halfPage(let down):
            let lines = count ?? max((buffer?.visibleLines ?? 2) / 2, 1)
            return vertical(down: down, lines: lines, firstNonBlank: false)
        case .lineStart:
            return Reach(target: t.lineStart(p), kind: .exclusive)
        case .firstNonBlank:
            return Reach(target: t.firstNonBlank(p), kind: .exclusive)
        case .lineEnd:
            var line = t.lineStart(p)
            for _ in 1..<n {
                guard let next = t.nextLineStart(line) else { break }
                line = next
            }
            return Reach(target: t.lastChar(line), kind: .inclusive)
        case .wordForward(let big):
            var q = p
            for _ in 0..<n { q = t.wordForward(q, big: big) }
            return q == p ? nil : Reach(target: q, kind: .exclusive)
        case .wordEnd(let big):
            var q = p
            for _ in 0..<n { q = t.wordEnd(q, big: big) }
            return q == p ? nil : Reach(target: q, kind: .inclusive)
        case .wordBackward(let big):
            var q = p
            for _ in 0..<n { q = t.wordBackward(q, big: big) }
            return q == p ? nil : Reach(target: q, kind: .exclusive)
        case .gotoLine(let last):
            let line = count.map { t.lineStart(number: $0) } ?? (last ? t.lastLineStart() : 0)
            return Reach(target: t.firstNonBlank(line), kind: .linewise)
        case .paragraph(let forward):
            let q = forward ? t.paragraphForward(p, count: n) : t.paragraphBackward(p, count: n)
            return q == p ? nil : Reach(target: q, kind: .exclusive)
        case .matchBracket:
            return t.matchBracket(p).map { Reach(target: $0, kind: .inclusive) }
        case .find(let c, let forward, let till):
            lastFind = (c, forward, till)
            return find(c, forward: forward, till: till, count: n, from: p, in: t)
        case .repeatFind(let reverse):
            guard let last = lastFind else { return nil }
            return find(last.char, forward: last.forward != reverse, till: last.till, count: n, from: p, in: t)
        }
    }

    private func find(_ c: Character, forward: Bool, till: Bool, count: Int, from p: Int, in t: VimText) -> Reach? {
        t.find(c, from: p, forward: forward, till: till, count: count)
            .map { Reach(target: $0, kind: forward ? .inclusive : .exclusive) }
    }

    // MARK: Operators

    /// Whole lines from the line of `a` through the line of `b`, terminators included.
    private func lineSpan(_ t: VimText, _ a: Int, _ b: Int) -> NSRange {
        let start = t.lineStart(min(a, b))
        return NSRange(location: start, length: NSMaxRange(t.lineRange(max(a, b))) - start)
    }

    private func operate(_ op: Operator, _ target: Target, count: Int?) {
        let t = text
        let p = cursor
        switch target {
        case .lines:
            var last = t.lineStart(p)
            for _ in 1..<max(count ?? 1, 1) {
                guard let next = t.nextLineStart(last) else { break }
                last = next
            }
            apply(op, lineSpan(t, p, last), linewise: true)
        case .object(let inner, let kind):
            guard let range = textObject(kind, inner: inner, at: p, in: t) else { return }
            apply(op, range, linewise: false)
        case .motion(var motion):
            // `cw` on a word changes to the end of it (`ce`, but stopping at the word under the cursor).
            var reach: Reach?
            if op == .change, case .wordForward(let big) = motion, p < t.length,
               ![.blank, .newline].contains(t.charClass(p)) {
                var q = p - 1
                for _ in 0..<max(count ?? 1, 1) { q = t.wordEnd(q, big: big) }
                motion = .wordEnd(big: big)
                reach = Reach(target: max(q, p), kind: .inclusive)
            } else {
                reach = evaluate(motion, count: count, from: p)
            }
            guard let reach else { return }
            var start = min(p, reach.target), end = max(p, reach.target)
            switch reach.kind {
            case .linewise:
                apply(op, lineSpan(t, start, end), linewise: true)
            case .inclusive:
                if end < t.contentEnd(end) { end = t.next(end) }
                apply(op, NSRange(location: start, length: end - start), linewise: false)
            case .exclusive:
                if case .wordForward = motion, t.lineStart(end) > t.lineStart(start) {
                    // `dw` on a line's last word stops at the end of the line.
                    end = max(start, t.contentEnd(t.lineStart(end) - 1))
                } else if end > start, end == t.lineStart(end), t.lineStart(end) > t.lineStart(start) {
                    end = t.contentEnd(end - 1)
                    if start <= t.firstNonBlank(start) {
                        // `d}` from the start of a line takes whole lines.
                        apply(op, lineSpan(t, start, end), linewise: true)
                        return
                    }
                }
                start = min(start, end)
                apply(op, NSRange(location: start, length: end - start), linewise: false)
            }
        }
    }

    private func apply(_ op: Operator, _ range: NSRange, linewise: Bool) {
        guard let buffer else { return }
        let t = text
        var yanked = t.s.substring(with: range)
        if linewise, !yanked.hasSuffix("\n") { yanked += "\n" }
        registers.store(.init(text: yanked, linewise: linewise), yank: op == .yank)
        switch op {
        case .yank:
            let keep = linewise && t.lineStart(range.location) == t.lineStart(cursor)
            setCursor(keep ? cursor : range.location)
        case .delete:
            var deleted = range
            if linewise, NSMaxRange(range) == t.length, range.length == 0 || !t.isNewline(t.length - 1), range.location > 0 {
                // The last line: take the newline before it instead.
                deleted = NSRange(location: range.location - 1, length: range.length + 1)
            }
            buffer.replace(deleted, with: "")
            if linewise {
                let after = text
                setCursor(after.firstNonBlank(min(deleted.location, after.length)))
            } else {
                setCursor(range.location)
            }
        case .change:
            if linewise {
                // Keep the first line's indentation and the last line's terminator.
                let indent = t.indent(range.location)
                let lastLine = t.lineStart(t.prev(NSMaxRange(range)))
                let end = max(t.contentEnd(lastLine), range.location)
                buffer.replace(NSRange(location: range.location, length: end - range.location), with: indent)
                startInsert(at: range.location + (indent as NSString).length)
            } else {
                buffer.replace(range, with: "")
                startInsert(at: range.location)
            }
        }
    }

    private func textObject(_ kind: Character, inner: Bool, at p: Int, in t: VimText) -> NSRange? {
        switch kind {
        case "w": t.wordObject(p, big: false, inner: inner)
        case "W": t.wordObject(p, big: true, inner: inner)
        case "\"": t.quoteObject(p, quote: 34, inner: inner)
        case "'": t.quoteObject(p, quote: 39, inner: inner)
        case "`": t.quoteObject(p, quote: 96, inner: inner)
        case "(", ")", "b": t.bracketObject(p, open: 40, close: 41, inner: inner)
        case "[", "]": t.bracketObject(p, open: 91, close: 93, inner: inner)
        case "{", "}", "B": t.bracketObject(p, open: 123, close: 125, inner: inner)
        case "<", ">": t.bracketObject(p, open: 60, close: 62, inner: inner)
        default: nil
        }
    }

    // MARK: Insert mode

    private func insert(_ kind: Character) {
        guard let buffer else { return }
        let t = text
        let p = cursor
        switch kind {
        case "i": startInsert(at: p)
        case "a": startInsert(at: p < t.contentEnd(p) ? t.next(p) : p)
        case "I": startInsert(at: t.firstNonBlank(p))
        case "A": startInsert(at: t.contentEnd(p))
        case "o":
            let indent = t.indent(p)
            let end = t.contentEnd(p)
            buffer.replace(NSRange(location: end, length: 0), with: "\n" + indent)
            startInsert(at: end + 1 + (indent as NSString).length)
        case "O":
            let indent = t.indent(p)
            let start = t.lineStart(p)
            buffer.replace(NSRange(location: start, length: 0), with: indent + "\n")
            startInsert(at: start + (indent as NSString).length)
        default: break
        }
    }

    private func startInsert(at p: Int) {
        guard let buffer else { return }
        mode = .insert
        buffer.select(NSRange(location: p, length: 0))
        insertStart = p
        insertLength = buffer.text.length
    }

    private func finishInsert() {
        guard let buffer else { return }
        let t = text
        let caret = buffer.selection.location
        if var change = recording {
            // What was typed, when it can be told from the text: the caret only moved forward over it.
            if caret >= insertStart, t.length - insertLength == caret - insertStart {
                change.inserted = t.s.substring(with: NSRange(location: insertStart, length: caret - insertStart))
            }
            lastChange = change
            recording = nil
        }
        mode = .normal
        setCursor(caret > t.lineStart(caret) ? t.prev(caret) : caret)
    }

    // MARK: Other edits

    private func replaceChars(with c: Character, count: Int) {
        let t = text
        let p = cursor, end = t.contentEnd(p)
        var q = p
        for _ in 0..<count {
            guard q < end else { return }
            q = t.next(q)
        }
        let replacement = String(repeating: String(c), count: count)
        buffer?.replace(NSRange(location: p, length: q - p), with: replacement)
        setCursor(p + (replacement as NSString).length - (String(c) as NSString).length)
    }

    private func toggleCase(count: Int) {
        let t = text
        let p = cursor, end = t.contentEnd(p)
        var q = p
        for _ in 0..<count where q < end { q = t.next(q) }
        guard q > p else { return }
        let range = NSRange(location: p, length: q - p)
        buffer?.replace(range, with: Self.toggled(t.s.substring(with: range)))
        setCursor(q < end ? q : t.prev(end))
    }

    private static func toggled(_ s: String) -> String {
        s.map { $0.isUppercase ? $0.lowercased() : $0.uppercased() }.joined()
    }

    /// `J`: joins `lines` lines into one, separated by a space.
    private func join(lines: Int) {
        for _ in 1..<lines {
            let t = text
            let start = t.lineStart(cursor)
            guard let next = t.nextLineStart(start) else { break }
            let end = t.contentEnd(start)
            let firstNonBlank = t.firstNonBlank(next)
            let nextEmpty = firstNonBlank == t.contentEnd(next)
            let noSpace = end == start || t.isBlank(end - 1) || nextEmpty
                || (firstNonBlank < t.length && t.unit(firstNonBlank) == 41)  // ")"
            let separator = noSpace ? "" : " "
            buffer?.replace(NSRange(location: end, length: firstNonBlank - end), with: separator)
            cursor = end
        }
        setCursor(cursor)
    }

    private func put(after: Bool, count: Int) {
        guard let buffer, let entry = registers.paste() else { return }
        let t = text
        let body = String(repeating: entry.text, count: count)
        if entry.linewise {
            var lines = body
            if !lines.hasSuffix("\n") { lines += "\n" }
            let at: Int
            if after {
                let range = t.lineRange(cursor)
                let end = NSMaxRange(range)
                if t.nextLineStart(cursor) == nil {
                    // The last line has no terminator to put the lines after.
                    buffer.replace(NSRange(location: end, length: 0), with: "\n" + lines.dropLast())
                    at = end + 1
                } else {
                    buffer.replace(NSRange(location: end, length: 0), with: lines)
                    at = end
                }
            } else {
                at = t.lineStart(cursor)
                buffer.replace(NSRange(location: at, length: 0), with: lines)
            }
            setCursor(text.firstNonBlank(at))
        } else {
            let at = after && cursor < t.contentEnd(cursor) ? t.next(cursor) : cursor
            buffer.replace(NSRange(location: at, length: 0), with: body)
            let end = at + (body as NSString).length
            setCursor(text.prev(end))
        }
    }

    private func undo(count: Int, redo: Bool) {
        guard let buffer else { return }
        for _ in 0..<count {
            if redo { buffer.redo() } else { buffer.undo() }
        }
        setCursor(buffer.selection.location)
    }

    private func repeatChange(count: Int?) {
        guard let buffer, let change = lastChange else { return }
        replaying = true
        defer { replaying = false }
        let countKeys = (count ?? change.count).map { String($0).map { VimKey.char($0) } } ?? []
        for key in countKeys + change.keys { handleNormal(key) }
        if mode == .insert {
            let caret = buffer.selection.location
            if let inserted = change.inserted {
                buffer.replace(NSRange(location: caret, length: 0), with: inserted)
                buffer.select(NSRange(location: caret + (inserted as NSString).length, length: 0))
            }
            finishInsert()
        }
    }

    // MARK: Visual mode

    private func visualRange(_ t: VimText) -> NSRange {
        if mode == .visualLine { return lineSpan(t, anchor, cursor) }
        let start = min(anchor, cursor), last = max(anchor, cursor)
        return NSRange(location: start, length: max(t.next(last), last) - start)
    }

    private func updateVisualSelection() {
        let range = visualRange(text)
        visualSelection = range
        buffer?.select(range)
    }

    private func exitVisual(to p: Int) {
        mode = .normal
        visualSelection = nil
        setCursor(p)
    }

    private static func parseVisual(_ keys: [VimKey]) -> Parsed<VimKey> {
        // Commands are one key; the rest are motions or `i`/`a` objects, decoded by `handleVisual`.
        var i = 0
        let count = parseCount(keys, &i)
        guard i < keys.count else { return .incomplete }
        let start = i
        if let object = parseObject(keys, &i) {
            switch object {
            case .incomplete: return .incomplete
            case .invalid: return .invalid
            case .object: return i == keys.count ? .done(keys[start], count: count) : .invalid
            }
        }
        if keys[i] == .escape || keys[i] == .ctrl("c") || keys[i] == .ctrl("[") {
            return i + 1 == keys.count ? .done(.escape, count: count) : .invalid
        }
        if case .char(let c) = keys[i], "vVodxXDyYcsCSRpPJ~uU".contains(c) {
            return i + 1 == keys.count ? .done(keys[i], count: count) : .invalid
        }
        switch parseMotion(keys, &i) {
        case .incomplete: return .incomplete
        case .invalid: return .invalid
        case .motion: return i == keys.count ? .done(keys[start], count: count) : .invalid
        }
    }

    private func handleVisual(_ key: VimKey) {
        guard let buffer else { return }
        let selection = buffer.selection
        if pending.isEmpty, selection != visualSelection {
            // The mouse changed the selection.
            if selection.length == 0 {
                mode = .normal
                visualSelection = nil
                handleNormal(key)
                return
            }
            if mode == .visualLine { mode = .visual }
            anchor = selection.location
            cursor = text.prev(NSMaxRange(selection))
        }
        pending.append(key)
        let keys = pending
        switch Self.parseVisual(keys) {
        case .incomplete: return
        case .invalid:
            pending = []
            return
        case .done(let first, let count):
            pending = []
            runVisual(first, count: count, keys: keys)
        }
    }

    private func runVisual(_ first: VimKey, count: Int?, keys: [VimKey]) {
        let t = text
        let range = visualRange(t)
        let start = range.location
        switch first {
        case .escape:
            exitVisual(to: cursor)
        case .char("v"):
            if mode == .visual { exitVisual(to: cursor) } else { mode = .visual; updateVisualSelection() }
        case .char("V"):
            if mode == .visualLine { exitVisual(to: cursor) } else { mode = .visualLine; updateVisualSelection() }
        case .char("o"):
            swap(&anchor, &cursor)
            updateVisualSelection()
        case .char("i"), .char("a"):
            guard case .char(let kind) = keys.last,
                  let object = textObject(kind, inner: first == .char("i"), at: cursor, in: t) else { return }
            mode = .visual
            anchor = object.location
            cursor = object.length > 0 ? t.prev(NSMaxRange(object)) : object.location
            updateVisualSelection()
        case .char(let c) where "dxXDyYcsCSR".contains(c):
            let op: Operator = "dxXD".contains(c) ? .delete : "yY".contains(c) ? .yank : .change
            let linewise = mode == .visualLine || "XDYCSR".contains(c)
            let target = linewise ? lineSpan(t, anchor, cursor) : range
            mode = .normal
            visualSelection = nil
            cursor = min(anchor, cursor)
            buffer?.undoGroup { apply(op, target, linewise: linewise) }
        case .char("p"), .char("P"):
            guard let entry = registers.paste() else { return exitVisual(to: start) }
            let linewise = mode == .visualLine
            var replacement = entry.text
            if linewise, !entry.linewise { replacement += "\n" }
            if !linewise, entry.linewise, replacement.hasSuffix("\n") { replacement = "\n" + replacement.dropLast() + "\n" }
            registers.store(.init(text: t.s.substring(with: range), linewise: linewise), yank: false)
            buffer?.replace(range, with: replacement)
            exitVisual(to: start)
        case .char("J"):
            var lines = 1, line = t.lineStart(start)
            while let next = t.nextLineStart(line), next <= t.lineStart(max(anchor, cursor)) {
                lines += 1
                line = next
            }
            mode = .normal
            visualSelection = nil
            cursor = t.lineStart(start)
            buffer?.undoGroup { join(lines: max(lines, 2)) }
        case .char(let c) where "~uU".contains(c):
            let selected = t.s.substring(with: range)
            let changed = c == "~" ? Self.toggled(selected) : c == "u" ? selected.lowercased() : selected.uppercased()
            buffer?.replace(range, with: changed)
            exitVisual(to: start)
        default:
            var i = 0
            _ = Self.parseCount(keys, &i)
            guard case .motion(let motion) = Self.parseMotion(keys, &i),
                  let reach = evaluate(motion, count: count, from: cursor) else { return }
            updateDesiredColumn(for: motion)
            cursor = t.clampNormal(reach.target)
            updateVisualSelection()
        }
    }
}
