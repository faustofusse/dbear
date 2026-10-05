import Foundation
import Testing
@testable import DBKit

@Test func highlightRangesAreUTF16() {
    let sql = "select '🐻 ñ' as x from public.users -- 🐻"
    let spans = SQLSyntax.highlight(sql)
    let ns = sql as NSString
    func text(_ kind: SyntaxKind) -> [String] { spans.filter { $0.kind == kind }.map { ns.substring(with: $0.range) } }
    #expect(text(.keyword).contains("select"))
    #expect(text(.string) == ["'🐻 ñ'"])
    #expect(text(.object).contains("users"))
    #expect(text(.comment) == ["-- 🐻"])
}

@Test func emptyTextHasNoSpans() {
    #expect(SQLSyntax.highlight("").isEmpty)
}

@Test func jsonKeysAreFieldsAndRangesAreUTF16() {
    let json = #"{"🐻": "ñ", "n": 1, "ok": null}"#
    let spans = JSONSyntax.highlight(json)
    let ns = json as NSString
    func text(_ kind: SyntaxKind) -> [String] { spans.filter { $0.kind == kind }.map { ns.substring(with: $0.range) } }
    #expect(text(.field) == [#""🐻""#, #""n""#, #""ok""#])
    #expect(text(.string) == [#""ñ""#])
    #expect(text(.number) == ["1"])
    #expect(text(.constant) == ["null"])
}
