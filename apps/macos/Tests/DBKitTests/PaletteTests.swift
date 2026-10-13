import Foundation
import Testing
@testable import DBKit

@Test func paletteRanksAndHighlightsInUTF16() {
    let search = PaletteSearch([
        PaletteItem(kind: .command, label: "New SQL Script", keywords: ["query"]),
        PaletteItem(kind: .table, label: "🐻_orders", keywords: ["public.🐻_orders", "app_dev"]),
        PaletteItem(kind: .table, label: "orders", keywords: ["public.orders", "app_dev"]),
        PaletteItem(kind: .connection, label: "app_dev", keywords: ["Local"]),
    ])
    let hits = search.rank("ord")
    #expect(hits.map(\.index) == [2, 1])
    #expect(hits[0].positions == [0, 1, 2])
    // The bear is two UTF-16 units, then "_": "orders" starts at 3.
    #expect(hits[1].positions == [3, 4, 5])
}

@Test func paletteEmptyQueryAndPrefixes() {
    let search = PaletteSearch([
        PaletteItem(kind: .command, label: "New SQL Script"),
        PaletteItem(kind: .table, label: "orders"),
        PaletteItem(kind: .connection, label: "app_dev"),
    ])
    #expect(search.rank("").map(\.index) == [0, 2])
    #expect(search.rank("#").map(\.index) == [1])
    #expect(search.rank(">s").map(\.index) == [0])
    #expect(search.rank("@dev").map(\.index) == [2])
    #expect(search.rank("zzz").isEmpty)
}
