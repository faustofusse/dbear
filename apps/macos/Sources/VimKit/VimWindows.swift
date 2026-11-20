import CoreGraphics

/// What follows ⌃W: Vim's window commands, over the app's panes (sidebars, editor, grids…).
public enum VimWindowCommand: Equatable, Sendable {
    public enum Direction: Sendable { case left, down, up, right }

    /// `h` `j` `k` `l` (or arrows, or with ⌃): the pane in that direction.
    case move(Direction)
    /// `w` / `⌃W`: the next pane, wrapping; `W` the previous one.
    case next, previous
    /// `p`: the pane focused before.
    case last
    /// `t` / `b`: the first or last pane.
    case top, bottom
    /// `o`: only this pane (hide the sidebars and the inspector).
    case only
    /// `c` / `q`: close (the tab).
    case close
    /// `n`: a new one (script).
    case new

    /// The command for the key after ⌃W; `nil` cancels it (⎋ or any other key).
    public init?(_ key: VimKey) {
        switch key {
        case .char("h"), .left, .ctrl("h"), .backspace: self = .move(.left)
        case .char("j"), .down, .ctrl("j"): self = .move(.down)
        case .char("k"), .up, .ctrl("k"): self = .move(.up)
        case .char("l"), .right, .ctrl("l"): self = .move(.right)
        case .char("w"), .ctrl("w"): self = .next
        case .char("W"): self = .previous
        case .char("p"), .ctrl("p"): self = .last
        case .char("t"), .ctrl("t"): self = .top
        case .char("b"), .ctrl("b"): self = .bottom
        case .char("o"), .ctrl("o"): self = .only
        case .char("c"), .char("q"), .ctrl("q"): self = .close
        case .char("n"), .ctrl("n"): self = .new
        default: return nil
        }
    }
}

/// Picks panes by their frames (top-left origin, y grows down), like Vim picks windows.
public enum VimPanes {
    /// Reading order: columns left to right, top to bottom within a column.
    public static func order(_ frames: [CGRect]) -> [Int] {
        frames.indices.sorted { a, b in
            let fa = frames[a], fb = frames[b]
            if abs(fa.minX - fb.minX) > 1 { return fa.minX < fb.minX }
            return fa.minY < fb.minY
        }
    }

    /// The pane `command` goes to from `current` (an index into `frames`, `nil` when no pane has focus).
    /// `nil` when there's nowhere to go.
    public static func target(_ command: VimWindowCommand, from current: Int?, in frames: [CGRect]) -> Int? {
        guard !frames.isEmpty else { return nil }
        let order = order(frames)
        guard let current, frames.indices.contains(current) else {
            if case .bottom = command { return order.last }
            return order.first
        }
        let position = order.firstIndex(of: current) ?? 0
        switch command {
        case .next: return order[(position + 1) % order.count]
        case .previous: return order[(position + order.count - 1) % order.count]
        case .top: return order.first
        case .bottom: return order.last
        case .move(let direction): return neighbor(of: current, toward: direction, in: frames)
        case .last, .only, .close, .new: return nil
        }
    }

    /// The nearest pane past `current`'s edge in `direction`: ones beside its middle first.
    static func neighbor(of current: Int, toward direction: VimWindowCommand.Direction, in frames: [CGRect]) -> Int? {
        let from = frames[current]
        let tolerance: CGFloat = 2
        var best: (index: Int, score: (CGFloat, CGFloat, CGFloat))?
        for (i, frame) in frames.enumerated() where i != current {
            // Distance past the edge, and how far the pane is from `from`'s middle across that axis.
            let gap: CGFloat
            let offset: CGFloat
            switch direction {
            case .left:
                gap = from.minX - frame.maxX
                offset = distance(from.midY, frame.minY, frame.maxY)
            case .right:
                gap = frame.minX - from.maxX
                offset = distance(from.midY, frame.minY, frame.maxY)
            case .up:
                gap = from.minY - frame.maxY
                offset = distance(from.midX, frame.minX, frame.maxX)
            case .down:
                gap = frame.minY - from.maxY
                offset = distance(from.midX, frame.minX, frame.maxX)
            }
            guard gap >= -tolerance else { continue }
            let score = (offset > 0 ? CGFloat(1) : 0, max(gap, 0), offset)
            if best == nil || score < best!.score { best = (i, score) }
        }
        return best?.index
    }

    private static func distance(_ x: CGFloat, _ lower: CGFloat, _ upper: CGFloat) -> CGFloat {
        x < lower ? lower - x : x > upper ? x - upper : 0
    }
}
