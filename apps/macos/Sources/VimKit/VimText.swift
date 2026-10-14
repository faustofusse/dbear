import Foundation

/// Read-only helpers over a text snapshot. Positions are UTF-16 offsets (like `NSRange`); every line,
/// including an empty last one after a trailing newline, counts as a line.
struct VimText {
    let s: NSString
    var length: Int { s.length }

    init(_ s: NSString) { self.s = s }

    enum CharClass { case blank, newline, word, punct }

    func unit(_ i: Int) -> unichar { s.character(at: i) }

    func isNewline(_ i: Int) -> Bool {
        guard i >= 0, i < length else { return false }
        let u = unit(i)
        return u == 10 || u == 13
    }

    func charClass(_ i: Int, big: Bool = false) -> CharClass {
        let u = unit(i)
        if u == 10 || u == 13 { return .newline }
        if u == 32 || u == 9 { return .blank }
        if u < 128 {
            let isWord = (u >= 48 && u <= 57) || (u >= 65 && u <= 90) || (u >= 97 && u <= 122) || u == 95
            if isWord { return .word }
            if u < 32 || u == 127 { return .blank }
            return big ? .word : .punct
        }
        if let scalar = Unicode.Scalar(u), scalar.properties.isWhitespace { return .blank }
        return .word
    }

    func isBlank(_ i: Int) -> Bool { i < length && charClass(i) == .blank }

    // MARK: Characters (composed sequences: emoji, combining marks, CRLF)

    func charRange(_ i: Int) -> NSRange {
        i >= length ? NSRange(location: length, length: 0) : s.rangeOfComposedCharacterSequence(at: i)
    }

    func charStart(_ i: Int) -> Int { i >= length ? length : charRange(i).location }

    func next(_ i: Int) -> Int { i >= length ? length : NSMaxRange(charRange(i)) }

    func prev(_ i: Int) -> Int { i <= 0 ? 0 : s.rangeOfComposedCharacterSequence(at: min(i, length) - 1).location }

    // MARK: Lines

    func lineRange(_ i: Int) -> NSRange { s.lineRange(for: NSRange(location: min(max(i, 0), length), length: 0)) }

    func lineStart(_ i: Int) -> Int { lineRange(i).location }

    /// End of the line's content, before its terminator.
    func contentEnd(_ i: Int) -> Int {
        let range = lineRange(i)
        var end = NSMaxRange(range)
        while end > range.location, isNewline(end - 1) { end -= 1 }
        return end
    }

    func isEmptyLine(_ i: Int) -> Bool { contentEnd(i) == lineStart(i) }

    func nextLineStart(_ i: Int) -> Int? {
        let range = lineRange(i)
        let end = NSMaxRange(range)
        return end > range.location && isNewline(end - 1) ? end : nil
    }

    func prevLineStart(_ i: Int) -> Int? {
        let start = lineStart(i)
        return start == 0 ? nil : lineStart(start - 1)
    }

    /// Start of line `number` (1-based), or of the last line if there are fewer.
    func lineStart(number: Int) -> Int {
        var start = 0
        for _ in 1..<max(number, 1) {
            guard let next = nextLineStart(start) else { break }
            start = next
        }
        return start
    }

    func lastLineStart() -> Int { lineStart(length) }

    func firstNonBlank(_ i: Int) -> Int {
        var p = lineStart(i)
        let end = contentEnd(i)
        while p < end, isBlank(p) { p += 1 }
        return p
    }

    /// Leading whitespace of the line.
    func indent(_ i: Int) -> String {
        let start = lineStart(i)
        return s.substring(with: NSRange(location: start, length: firstNonBlank(i) - start))
    }

    /// The last character of the line (its start), or the line start when it's empty.
    func lastChar(_ i: Int) -> Int {
        let start = lineStart(i), end = contentEnd(i)
        return end > start ? prev(end) : start
    }

    /// Characters between the line start and `i`.
    func column(_ i: Int) -> Int {
        var p = lineStart(i), column = 0
        while p < i { p = next(p); column += 1 }
        return column
    }

    /// The character at `column` on the line starting at `start`, clamped to its last character.
    func position(lineStart start: Int, column: Int) -> Int {
        let end = contentEnd(start)
        var p = start, c = 0
        while c < column, p < end {
            let n = next(p)
            if n >= end { break }
            p = n
            c += 1
        }
        return p
    }

    /// Where the cursor can sit in normal mode: on a character, not on a line's terminator
    /// (unless the line is empty).
    func clampNormal(_ i: Int) -> Int {
        let p = min(max(i, 0), length)
        let start = lineStart(p), end = contentEnd(p)
        if p >= end { return end > start ? prev(end) : start }
        return charStart(p)
    }

    // MARK: Words

    func wordForward(_ p: Int, big: Bool) -> Int {
        var i = p
        guard i < length else { return length }
        let c = charClass(i, big: big)
        if c == .word || c == .punct {
            while i < length, charClass(i, big: big) == c { i += 1 }
        }
        while i < length {
            switch charClass(i, big: big) {
            case .blank: i += 1
            case .newline:
                i = next(i)
                // An empty line is a word of its own.
                if i < length, charClass(i) == .newline { return i }
            default: return i
            }
        }
        return i
    }

    func wordEnd(_ p: Int, big: Bool) -> Int {
        var i = p + 1
        while i < length, [.blank, .newline].contains(charClass(i, big: big)) { i += 1 }
        guard i < length else { return p }
        let c = charClass(i, big: big)
        while i + 1 < length, charClass(i + 1, big: big) == c { i += 1 }
        return charStart(i)
    }

    func wordBackward(_ p: Int, big: Bool) -> Int {
        guard p > 0 else { return 0 }
        var i = p - 1
        while i > 0 {
            switch charClass(i, big: big) {
            case .blank: i -= 1
            case .newline:
                if isNewline(i - 1) { return charStart(i) }  // empty line
                i -= 1
            default:
                let c = charClass(i, big: big)
                while i > 0, charClass(i - 1, big: big) == c { i -= 1 }
                return i
            }
        }
        return 0
    }

    // MARK: Paragraphs, brackets, finds

    func paragraphForward(_ p: Int, count: Int) -> Int {
        var line = lineStart(p)
        for _ in 0..<count {
            while isEmptyLine(line), let next = nextLineStart(line) { line = next }
            while !isEmptyLine(line) {
                guard let next = nextLineStart(line) else { return contentEnd(line) }
                line = next
            }
        }
        return line
    }

    func paragraphBackward(_ p: Int, count: Int) -> Int {
        var line = lineStart(p)
        for _ in 0..<count {
            while isEmptyLine(line), let prev = prevLineStart(line) { line = prev }
            while !isEmptyLine(line) {
                guard let prev = prevLineStart(line) else { return 0 }
                line = prev
            }
        }
        return line
    }

    private static let pairs: [unichar: (unichar, Bool)] = [
        40: (41, true), 41: (40, false),    // ( )
        91: (93, true), 93: (91, false),    // [ ]
        123: (125, true), 125: (123, false), // { }
    ]

    /// `%`: the bracket matching the first one at or after `p` on its line.
    func matchBracket(_ p: Int) -> Int? {
        var i = p
        let end = contentEnd(p)
        while i < end, Self.pairs[unit(i)] == nil { i += 1 }
        guard i < end, let (other, forward) = Self.pairs[unit(i)] else { return nil }
        let open = unit(i)
        var depth = 0
        var j = i
        while true {
            j += forward ? 1 : -1
            guard j >= 0, j < length else { return nil }
            let u = unit(j)
            if u == open { depth += 1 } else if u == other {
                if depth == 0 { return j }
                depth -= 1
            }
        }
    }

    private func matches(_ i: Int, _ ch: Character) -> Bool {
        s.substring(with: charRange(i)) == String(ch)
    }

    /// `f`/`F`/`t`/`T`: the `count`th `ch` after (or before) `p` on its line.
    func find(_ ch: Character, from p: Int, forward: Bool, till: Bool, count: Int) -> Int? {
        let start = lineStart(p), end = contentEnd(p)
        var i = p, found = 0
        // `t`/`T` repeated with `;` shouldn't stick on the character right next to the cursor.
        if till {
            let adjacent = forward ? next(p) : prev(p)
            if adjacent != p, adjacent >= start, adjacent < end, matches(adjacent, ch) { i = adjacent }
        }
        while found < count {
            if forward {
                i = next(i)
                guard i < end else { return nil }
            } else {
                guard i > start else { return nil }
                i = prev(i)
            }
            if matches(i, ch) { found += 1 }
        }
        if till { return forward ? prev(i) : next(i) }
        return i
    }

    // MARK: Text objects

    func wordObject(_ p: Int, big: Bool, inner: Bool) -> NSRange? {
        let start = lineStart(p), end = contentEnd(p)
        guard end > start else { return nil }
        let q = min(p, end - 1)
        let c = charClass(q, big: big)
        var s = q, f = q + 1
        while s > start, charClass(s - 1, big: big) == c { s -= 1 }
        while f < end, charClass(f, big: big) == c { f += 1 }
        if inner { return NSRange(location: s, length: f - s) }
        if c == .blank {
            if f < end {
                let c2 = charClass(f, big: big)
                while f < end, charClass(f, big: big) == c2 { f += 1 }
            }
            return NSRange(location: s, length: f - s)
        }
        var g = f
        while g < end, isBlank(g) { g += 1 }
        if g > f { return NSRange(location: s, length: g - s) }
        var h = s
        while h > start, isBlank(h - 1) { h -= 1 }
        return NSRange(location: h, length: f - h)
    }

    func quoteObject(_ p: Int, quote: unichar, inner: Bool) -> NSRange? {
        let start = lineStart(p), end = contentEnd(p)
        var quotes: [Int] = []
        for i in start..<end where unit(i) == quote { quotes.append(i) }
        var pair: (Int, Int)?
        for k in stride(from: 0, to: quotes.count - 1, by: 2) {
            let (a, b) = (quotes[k], quotes[k + 1])
            if a <= p, p <= b { pair = (a, b); break }
            if a > p, pair == nil { pair = (a, b); break }
        }
        guard let (a, b) = pair else { return nil }
        if inner { return NSRange(location: a + 1, length: b - a - 1) }
        // Like `aw`: trailing whitespace, or leading whitespace when there's none after.
        var s = a, f = b + 1
        while f < end, isBlank(f) { f += 1 }
        if f == b + 1 { while s > start, isBlank(s - 1) { s -= 1 } }
        return NSRange(location: s, length: f - s)
    }

    func bracketObject(_ p: Int, open: unichar, close: unichar, inner: Bool) -> NSRange? {
        guard p < length else { return nil }
        var o: Int?
        if unit(p) == open {
            o = p
        } else {
            var depth = 0
            var i = p - 1  // on `close`, this finds its own `open`
            while i >= 0 {
                let u = unit(i)
                if u == close { depth += 1 } else if u == open {
                    if depth == 0 { o = i; break }
                    depth -= 1
                }
                i -= 1
            }
        }
        guard let o else { return nil }
        var depth = 0
        var i = o + 1
        while i < length {
            let u = unit(i)
            if u == open { depth += 1 } else if u == close {
                if depth == 0 { break }
                depth -= 1
            }
            i += 1
        }
        guard i < length else { return nil }
        return inner ? NSRange(location: o + 1, length: i - o - 1) : NSRange(location: o, length: i - o + 1)
    }
}
