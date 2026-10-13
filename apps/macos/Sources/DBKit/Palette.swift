import DBCoreFFI
import Foundation

/// What a command palette entry is: decides the prefix that finds it (`>` commands, `#` tables,
/// `@` connections) and what an empty query lists (everything but tables).
public enum PaletteItemKind: Sendable, Hashable {
    case command, table, connection, tab
}

/// Something the command palette can pick, as searched: the label and other words that find it.
public struct PaletteItem: Sendable, Hashable {
    public var kind: PaletteItemKind
    public var label: String
    public var keywords: [String]

    public init(kind: PaletteItemKind, label: String, keywords: [String] = []) {
        self.kind = kind
        self.label = label
        self.keywords = keywords
    }
}

public struct PaletteHit: Sendable, Hashable {
    /// Index into the items the search was made with.
    public let index: Int
    /// UTF-16 offsets of the label's matched characters (to highlight them).
    public let positions: [Int]
}

/// The palette's fuzzy search (`dbcore::palette`, shared with the GPUI app). Made once when the
/// palette opens; `rank` runs on each keystroke.
public final class PaletteSearch: @unchecked Sendable {
    private let palette: DBCoreFFI.Palette

    public init(_ items: [PaletteItem]) {
        palette = DBCoreFFI.Palette(candidates: items.map {
            DBCoreFFI.PaletteCandidate(kind: $0.kind.ffi, label: $0.label, keywords: $0.keywords)
        })
    }

    /// The items matching `query`, best first, at most `limit`.
    public func rank(_ query: String, limit: Int = 60) -> [PaletteHit] {
        palette.rank(query: query, limit: UInt32(limit)).map {
            PaletteHit(index: Int($0.index), positions: $0.positions.map(Int.init))
        }
    }
}

private extension PaletteItemKind {
    var ffi: DBCoreFFI.PaletteKind {
        switch self {
        case .command: .command
        case .table: .table
        case .connection: .connection
        case .tab: .tab
        }
    }
}
