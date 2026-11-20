import CoreGraphics
import Testing
@testable import VimKit

/// The main window: connections | tables | editor over results, with the inspector on the right.
private let sidebar = CGRect(x: 0, y: 0, width: 200, height: 800)
private let tables = CGRect(x: 200, y: 0, width: 250, height: 800)
private let editor = CGRect(x: 450, y: 50, width: 600, height: 300)
private let results = CGRect(x: 450, y: 350, width: 600, height: 450)
private let inspector = CGRect(x: 1050, y: 0, width: 300, height: 800)
private let frames = [results, inspector, sidebar, editor, tables]  // not in order
private func index(_ frame: CGRect) -> Int { frames.firstIndex(of: frame)! }

@Test func windowCommandKeys() {
    #expect(VimWindowCommand(.char("h")) == .move(.left))
    #expect(VimWindowCommand(.ctrl("l")) == .move(.right))
    #expect(VimWindowCommand(.down) == .move(.down))
    #expect(VimWindowCommand(.ctrl("w")) == .next)
    #expect(VimWindowCommand(.char("W")) == .previous)
    #expect(VimWindowCommand(.escape) == nil)
    #expect(VimWindowCommand(.char("z")) == nil)
}

@Test func paneOrder() {
    #expect(VimPanes.order(frames) == [sidebar, tables, editor, results, inspector].map(index))
}

@Test func paneDirections() {
    func go(_ d: VimWindowCommand.Direction, from f: CGRect) -> CGRect? {
        VimPanes.target(.move(d), from: index(f), in: frames).map { frames[$0] }
    }
    #expect(go(.down, from: editor) == results)
    #expect(go(.up, from: results) == editor)
    #expect(go(.up, from: editor) == nil)
    #expect(go(.left, from: editor) == tables)
    #expect(go(.left, from: tables) == sidebar)
    #expect(go(.left, from: sidebar) == nil)
    #expect(go(.right, from: tables) == results)  // beside the middle of the tables list
    #expect(go(.right, from: editor) == inspector)
    #expect(go(.left, from: inspector) == results)
}

@Test func paneCycling() {
    func run(_ c: VimWindowCommand, from f: CGRect?) -> CGRect? {
        VimPanes.target(c, from: f.map(index), in: frames).map { frames[$0] }
    }
    #expect(run(.next, from: editor) == results)
    #expect(run(.next, from: inspector) == sidebar)
    #expect(run(.previous, from: sidebar) == inspector)
    #expect(run(.top, from: results) == sidebar)
    #expect(run(.bottom, from: results) == inspector)
    #expect(run(.next, from: nil) == sidebar)
    #expect(VimPanes.target(.next, from: nil, in: []) == nil)
}

@MainActor
@Test func switchTabKeys() {
    let vim = Vim("select 1|")
    var calls: [String] = []
    vim.engine.onSwitchTab = { forward, count in calls.append("\(forward ? "gt" : "gT")\(count.map(String.init) ?? "")") }
    vim.keys("gt").keys("gT").keys("3gt").keys("2gT").keys("gg")
    #expect(calls == ["gt", "gT", "gt3", "gT2"])
    #expect(vim.state == "|select 1")
}
