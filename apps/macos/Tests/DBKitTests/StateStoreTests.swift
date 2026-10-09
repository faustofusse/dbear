import Foundation
import Testing
@testable import DBKit

@Test func stateStoreKeepsValuesAndHistoryBesideTheConnections() throws {
    let dir = FileManager.default.temporaryDirectory.appendingPathComponent("dbear-tests-\(UUID().uuidString)")
    defer { try? FileManager.default.removeItem(at: dir) }
    let store = try ConnectionStore.open(path: dir.appendingPathComponent("dbear.db").path)
    let state = try StateStore.open(besideStoreAt: store.path)
    #expect(state.path.hasSuffix("/state.db"))

    #expect(state.value(forKey: "macos.session") == nil)
    try state.setValue("{\"tabs\":[]}", forKey: "macos.session")
    #expect(state.value(forKey: "macos.session") == "{\"tabs\":[]}")
    try state.setValue(nil, forKey: "macos.session")
    #expect(state.value(forKey: "macos.session") == nil)

    try state.addHistory(connectionID: "a", database: "app", sql: "select 1", duration: .milliseconds(12), rows: 1, error: nil)
    try state.addHistory(connectionID: "a", database: "app", sql: "select nope", duration: nil, rows: nil, error: "no column")
    try state.addHistory(connectionID: "b", database: "", sql: "select 2", duration: nil, rows: nil, error: nil)
    // Running it again moves it to the top instead of adding a copy.
    try state.addHistory(connectionID: "a", database: "app", sql: "select 1", duration: .milliseconds(3), rows: 1, error: nil)

    let history = try state.history(of: "a")
    #expect(history.map(\.sql) == ["select 1", "select nope"])
    #expect(history[0].duration == .milliseconds(3) && history[0].rows == 1 && history[0].error == nil)
    #expect(history[1].error == "no column")

    try state.clearHistory(of: "a")
    #expect(try state.history(of: "a").isEmpty)
    #expect(try state.history(of: "b").count == 1)
}
