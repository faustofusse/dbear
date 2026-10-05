import DBCoreFFI
import Foundation

/// Token categories produced by the core's tree-sitter highlighters.
public enum SyntaxKind: Sendable, Hashable, CaseIterable {
    case keyword, type, object, function, field, variable, parameter
    case string, number, constant, comment, `operator`, punctuation
}

public struct SyntaxSpan: Sendable, Hashable {
    /// UTF-16 range, directly usable on `NSString` / `NSTextStorage`.
    public let range: NSRange
    public let kind: SyntaxKind
}

public enum SQLSyntax {
    /// Spans sorted by location; apply in order (later spans are more specific).
    public static func highlight(_ text: String) -> [SyntaxSpan] {
        DBCoreFFI.highlightSql(text: text).map {
            SyntaxSpan(range: NSRange(location: Int($0.location), length: Int($0.length)), kind: SyntaxKind($0.kind))
        }
    }
}

public enum JSONSyntax {
    /// Spans sorted by location; object keys are `.field`, `true`/`false`/`null` are `.constant`.
    public static func highlight(_ text: String) -> [SyntaxSpan] {
        DBCoreFFI.highlightJson(text: text).map {
            SyntaxSpan(range: NSRange(location: Int($0.location), length: Int($0.length)), kind: SyntaxKind($0.kind))
        }
    }
}

private extension SyntaxKind {
    init(_ kind: DBCoreFFI.HighlightKind) {
        switch kind {
        case .keyword: self = .keyword
        case .type: self = .type
        case .object: self = .object
        case .function: self = .function
        case .field: self = .field
        case .variable: self = .variable
        case .parameter: self = .parameter
        case .string: self = .string
        case .number: self = .number
        case .constant: self = .constant
        case .comment: self = .comment
        case .operator: self = .operator
        case .punctuation: self = .punctuation
        }
    }
}
