import Foundation
import Testing
@testable import VimKit

/// An in-memory `VimBuffer` with a simple undo stack.
@MainActor
final class TestBuffer: VimBuffer {
    private var storage: NSMutableString
    var selection: NSRange
    private var undoStack: [[(NSRange, String, String)]] = []
    private var redoStack: [[(NSRange, String, String)]] = []
    private var group: [(NSRange, String, String)]?
    var visibleLines = 10

    init(_ text: String, caret: Int = 0) {
        storage = NSMutableString(string: text)
        selection = NSRange(location: caret, length: 0)
    }

    var text: NSString { NSString(string: storage) }
    var string: String { storage as String }

    func select(_ range: NSRange) { selection = range }

    func replace(_ range: NSRange, with string: String) {
        let old = storage.substring(with: range)
        storage.replaceCharacters(in: range, with: string)
        let edit = (range, old, string)
        if group != nil { group!.append(edit) } else { undoStack.append([edit]) }
        redoStack = []
        selection = NSRange(location: range.location + (string as NSString).length, length: 0)
    }

    func undoGroup(_ body: () -> Void) {
        if group != nil { return body() }
        group = []
        body()
        if let group, !group.isEmpty { undoStack.append(group) }
        group = nil
    }

    func undo() {
        guard let edits = undoStack.popLast() else { return }
        for (range, old, new) in edits.reversed() {
            storage.replaceCharacters(in: NSRange(location: range.location, length: (new as NSString).length), with: old)
        }
        redoStack.append(edits)
        selection = NSRange(location: edits.first!.0.location, length: 0)
    }

    func redo() {
        guard let edits = redoStack.popLast() else { return }
        for (range, _, new) in edits { storage.replaceCharacters(in: range, with: new) }
        undoStack.append(edits)
        selection = NSRange(location: edits.first!.0.location, length: 0)
    }

    /// Typing in insert mode (the text view's job in the app).
    func type(_ s: String) {
        let range = selection
        storage.replaceCharacters(in: range, with: s)
        selection = NSRange(location: range.location + (s as NSString).length, length: 0)
    }
}

@MainActor
final class TestClipboard: VimClipboard {
    var changeCount = 0
    var contents: String?
    func read() -> String? { contents }
    func write(_ text: String) { contents = text; changeCount += 1 }
}

@MainActor
struct Vim {
    let buffer: TestBuffer
    let engine: VimEngine
    let clipboard = TestClipboard()

    /// `|` in `text` marks the caret.
    init(_ text: String) {
        let caret = (text as NSString).range(of: "|").location
        buffer = TestBuffer(text.replacingOccurrences(of: "|", with: ""), caret: caret == NSNotFound ? 0 : caret)
        engine = VimEngine(buffer: buffer, registers: VimRegisters(clipboard: clipboard))
    }

    /// Feeds keys: plain characters, `<Esc>`, `<CR>`, `<BS>`, `<C-x>`.
    @discardableResult
    func keys(_ s: String) -> Vim {
        var rest = Substring(s)
        while let c = rest.first {
            if c == "<", let end = rest.firstIndex(of: ">") {
                let name = rest[rest.index(after: rest.startIndex)..<end]
                rest = rest[rest.index(after: end)...]
                switch name {
                case "Esc": engine.handle(.escape)
                case "CR": engine.handle(.enter)
                case "BS": engine.handle(.backspace)
                default:
                    if name.hasPrefix("C-") { engine.handle(.ctrl(name.last!)) }
                }
                continue
            }
            rest = rest.dropFirst()
            if engine.mode == .insert { buffer.type(String(c)) } else { engine.handle(.char(c)) }
        }
        return self
    }

    /// The text with `|` at the caret (or `[…]` around the selection).
    var state: String {
        let s = NSMutableString(string: buffer.string)
        let range = buffer.selection
        if range.length > 0 {
            s.insert("]", at: NSMaxRange(range))
            s.insert("[", at: range.location)
        } else {
            s.insert("|", at: range.location)
        }
        return s as String
    }
}

@MainActor
@Test func basicMotions() {
    #expect(Vim("|select a, b\nfrom t").keys("l").state == "s|elect a, b\nfrom t")
    #expect(Vim("|select a, b\nfrom t").keys("w").state == "select |a, b\nfrom t")
    #expect(Vim("|select a, b\nfrom t").keys("3w").state == "select a, |b\nfrom t")
    #expect(Vim("|select a, b\nfrom t").keys("W").state == "select |a, b\nfrom t")
    #expect(Vim("|select a, b\nfrom t").keys("e").state == "selec|t a, b\nfrom t")
    #expect(Vim("select a, |b\nfrom t").keys("b").state == "select a|, b\nfrom t")
    #expect(Vim("select a, |b\nfrom t").keys("B").state == "select |a, b\nfrom t")
    #expect(Vim("|select a, b\nfrom t").keys("$").state == "select a, |b\nfrom t")
    #expect(Vim("  sel|ect").keys("0").state == "|  select")
    #expect(Vim("  sel|ect").keys("^").state == "  |select")
    #expect(Vim("|abc").keys("10l").state == "ab|c")
    #expect(Vim("a|bc").keys("h").state == "|abc")
}

@MainActor
@Test func verticalMotionsKeepTheColumn() {
    let vim = Vim("abcdef|\nab\nabcdef")
    #expect(vim.keys("j").state == "abcdef\na|b\nabcdef")
    #expect(vim.keys("j").state == "abcdef\nab\nabcde|f")
    #expect(vim.keys("gg").state == "|abcdef\nab\nabcdef")
    #expect(vim.keys("G").state == "abcdef\nab\n|abcdef")
    #expect(vim.keys("2G").state == "abcdef\n|ab\nabcdef")
    #expect(vim.keys("k$jj").state == "abcdef\nab\nabcde|f")
}

@MainActor
@Test func wordsCrossLinesAndStopOnEmptyLines() {
    #expect(Vim("|foo\n\nbar").keys("w").state == "foo\n|\nbar")
    #expect(Vim("|foo\n\nbar").keys("ww").state == "foo\n\n|bar")
    #expect(Vim("foo\n\n|bar").keys("b").state == "foo\n|\nbar")
    #expect(Vim("foo\n\n|bar").keys("bb").state == "|foo\n\nbar")
}

@MainActor
@Test func findsAndBrackets() {
    #expect(Vim("|select f(a, b)").keys("f(").state == "select f|(a, b)")
    #expect(Vim("|select f(a, b)").keys("t,").state == "select f(|a, b)")
    #expect(Vim("|a,b,c,d").keys("f,;").state == "a,b|,c,d")
    #expect(Vim("|a,b,c,d").keys("f,;,").state == "a|,b,c,d")
    #expect(Vim("select f|(a, (b))").keys("%").state == "select f(a, (b)|)")
    #expect(Vim("select f(a, (b)|)").keys("%").state == "select f|(a, (b))")
    #expect(Vim("a\nb\n\nc\nd|").keys("{").state == "a\nb\n|\nc\nd")
    #expect(Vim("|a\nb\n\nc").keys("}").state == "a\nb\n|\nc")
}

@MainActor
@Test func deleteChangeYank() {
    #expect(Vim("|select a from t").keys("dw").state == "|a from t")
    #expect(Vim("select |a\nfrom t").keys("dw").state == "select| \nfrom t")
    #expect(Vim("|select a from t").keys("d2w").state == "|from t")
    #expect(Vim("|select a from t").keys("2dw").state == "|from t")
    #expect(Vim("select a| from t").keys("x").state == "select a|from t")
    #expect(Vim("select a from |t").keys("x").state == "select a from| ")
    #expect(Vim("select |a from t").keys("D").state == "select| ")
    #expect(Vim("select |abc from t").keys("cwxyz<Esc>").state == "select xy|z from t")
    #expect(Vim("select ab|c from t").keys("cwd<Esc>").state == "select ab|d from t")
    #expect(Vim("select |a from t").keys("Cb<Esc>").state == "select |b")
    #expect(Vim("select a from t|").keys("dF ").state == "select a from|t")
    #expect(Vim("|select a from t").keys("de").state == "| a from t")
    #expect(Vim("|select a from t").keys("dfa").state == "| from t")
    #expect(Vim("|select a from t").keys("dta").state == "|a from t")
}

@MainActor
@Test func linewiseOperators() {
    #expect(Vim("a\n|b\nc").keys("dd").state == "a\n|c")
    #expect(Vim("a\nb\n|c").keys("dd").state == "a\n|b")
    #expect(Vim("|a\nb\nc").keys("2dd").state == "|c")
    #expect(Vim("|a\nb\nc").keys("dj").state == "|c")
    #expect(Vim("a\nb\n|c").keys("dk").state == "|a")
    #expect(Vim("a\nb\n|c").keys("dgg").state == "|")
    #expect(Vim("|a\nb\nc").keys("dG").state == "|")
    #expect(Vim("a\n  |b\nc").keys("ccx<Esc>").state == "a\n  |x\nc")
    #expect(Vim("a\n|b\nc").keys("yyp").state == "a\nb\n|b\nc")
    #expect(Vim("a\n|b\nc").keys("yyP").state == "a\n|b\nb\nc")
    #expect(Vim("a\nb\n|c").keys("yyp").state == "a\nb\nc\n|c")
    #expect(Vim("a\nb\n|c").keys("ddP").state == "a\n|c\nb")
    #expect(Vim("|a\n\nb").keys("d}").state == "|\nb")
}

@MainActor
@Test func textObjects() {
    #expect(Vim("select f|oo, bar").keys("diw").state == "select |, bar")
    #expect(Vim("select f|oo, bar").keys("daw").state == "select|, bar")
    #expect(Vim("select f|oo bar").keys("daw").state == "select |bar")
    #expect(Vim("where a = 'he|llo'").keys("ci'x<Esc>").state == "where a = '|x'")
    #expect(Vim("where a = 'he|llo'").keys("da'").state == "where a |=")
    #expect(Vim("where |a = 'hello'").keys("di'").state == "where a = '|'")
    #expect(Vim("count(a, f(|b))").keys("di(").state == "count(a, f(|))")
    #expect(Vim("count(a, f|(b))").keys("dib").state == "count(a, f(|))")
    #expect(Vim("count(|a, f(b))").keys("da(").state == "coun|t")
    #expect(Vim("x = [1, |2]").keys("ci[9<Esc>").state == "x = [|9]")
}

@MainActor
@Test func insertCommands() {
    #expect(Vim("a|bc").keys("ix<Esc>").state == "a|xbc")
    #expect(Vim("a|bc").keys("ax<Esc>").state == "ab|xc")
    #expect(Vim("  a|bc").keys("Ix<Esc>").state == "  |xabc")
    #expect(Vim("a|bc").keys("Ax<Esc>").state == "abc|x")
    #expect(Vim("  a|bc\nd").keys("ox<Esc>").state == "  abc\n  |x\nd")
    #expect(Vim("  a|bc\nd").keys("Ox<Esc>").state == "  |x\n  abc\nd")
    let vim = Vim("a|bc")
    vim.keys("i")
    #expect(vim.engine.mode == .insert)
    vim.keys("<Esc>")
    #expect(vim.engine.mode == .normal)
}

@MainActor
@Test func otherEdits() {
    #expect(Vim("|abc").keys("rx").state == "|xbc")
    #expect(Vim("|abc").keys("2rx").state == "x|xc")
    #expect(Vim("|abc").keys("4rx").state == "|abc")
    #expect(Vim("|abc").keys("~~").state == "AB|c")
    #expect(Vim("|select\n  a,\n  b").keys("J").state == "select| a,\n  b")
    #expect(Vim("|select\n  a,\n  b").keys("3J").state == "select a,| b")
    #expect(Vim("|f(\n)").keys("J").state == "f(|)")
    #expect(Vim("a|bc").keys("xp").state == "ac|b")
    #expect(Vim("a|bc").keys("yl3p").state == "abbb|bc")
}

@MainActor
@Test func undoRedoAndRepeat() {
    let vim = Vim("|one two three four")
    vim.keys("dw")
    #expect(vim.state == "|two three four")
    vim.keys("u")
    #expect(vim.state == "|one two three four")
    vim.keys("<C-r>")
    #expect(vim.state == "|two three four")
    vim.keys(".")
    #expect(vim.state == "|three four")
    #expect(Vim("|a b c d e").keys("dw2.").state == "|d e")

    let change = Vim("|foo bar baz")
    change.keys("cwx<Esc>w.")
    #expect(change.state == "x |x baz")
    #expect(Vim("|a\nb\nc").keys("Ax;<Esc>j.j.").state == "ax;\nbx;\ncx|;")
}

@MainActor
@Test func visualMode() {
    #expect(Vim("|select a from t").keys("v").state == "[s]elect a from t")
    #expect(Vim("|select a from t").keys("ve").state == "[select] a from t")
    #expect(Vim("|select a from t").keys("ved").state == "| a from t")
    #expect(Vim("select |abc from t").keys("veo").state == "select [abc] from t")
    #expect(Vim("a\n|b\nc").keys("V").state == "a\n[b\n]c")
    #expect(Vim("a\n|b\nc").keys("Vjd").state == "|a")
    #expect(Vim("select f(|a, b)").keys("vi(").state == "select f([a, b])")
    #expect(Vim("|select a").keys("veU").state == "|SELECT a")
    #expect(Vim("|select a").keys("vey").state == "|select a")
    #expect(Vim("|select a").keys("vec*<Esc>").state == "|* a")
    #expect(Vim("|a\nb\nc").keys("VjJ").state == "a| b\nc")
    #expect(Vim("|select a").keys("v<Esc>").state == "|select a")

    // A mouse selection in normal mode continues in visual mode.
    let vim = Vim("select a from t")
    vim.buffer.select(NSRange(location: 7, length: 6))
    vim.keys("d")
    #expect(vim.state == "select | t")
}

@MainActor
@Test func registersUseTheClipboard() {
    let vim = Vim("|select a")
    vim.keys("yw")
    #expect(vim.clipboard.contents == "select ")
    vim.clipboard.write("t\n")
    vim.keys("p")
    #expect(vim.state == "select a\n|t")
    // Deletes don't touch the clipboard.
    vim.keys("x")
    #expect(vim.clipboard.contents == "t\n")
}

@MainActor
@Test func emptyAndUnicodeText() {
    #expect(Vim("|").keys("xdddwjk").state == "|")
    #expect(Vim("|🐻 bear").keys("l").state == "🐻| bear")
    #expect(Vim("|🐻 bear").keys("x").state == "| bear")
    #expect(Vim("|ñandú 🐻").keys("e").state == "ñand|ú 🐻")
    #expect(Vim("ab|c\r\nd").keys("j").state == "abc\r\n|d")
}

@MainActor
@Test func pendingKeysAndCounts() {
    let vim = Vim("|a b c")
    vim.keys("2d")
    #expect(vim.engine.pendingKeys == "2d")
    vim.keys("<Esc>")
    #expect(vim.engine.pendingKeys == "")
    #expect(vim.state == "|a b c")
}
