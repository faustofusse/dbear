import AppKit
import SwiftUI

extension View {
    /// Builds the enclosing `List`'s row views once, up front. A List only makes views for the rows
    /// on screen; the first time a section folds, the rows sliding into view need brand-new views,
    /// which pop straight into place while the folding rows slide over them. Built once (and then
    /// recycled by the table), they animate like the rest. `token` changes when the rows change.
    func preparesListRows(_ token: some Hashable) -> some View {
        background(ListRowPreparer(token: AnyHashable(token)))
    }
}

private struct ListRowPreparer: NSViewRepresentable {
    let token: AnyHashable

    /// Enough spare views for a fold to fill the column; bigger lists don't need them all.
    private static let maxRows = 200

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        guard context.coordinator.token != token else { return }
        context.coordinator.token = token
        prepare(near: view, attempts: 10)
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var token: AnyHashable?
    }

    /// Runs once the List has loaded the new rows into its table (it may not be in the window yet).
    private func prepare(near view: NSView, attempts: Int) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) { [weak view] in
            guard let view else { return }
            guard let table = Self.table(near: view), table.numberOfRows > 0 else {
                if attempts > 1 { prepare(near: view, attempts: attempts - 1) }
                return
            }
            for row in 0..<min(table.numberOfRows, Self.maxRows) {
                _ = table.rowView(atRow: row, makeIfNecessary: true)
            }
        }
    }

    /// The closest table view: the List's, which shares an ancestor with this background view.
    private static func table(near view: NSView) -> NSTableView? {
        var ancestor = view.superview
        while let current = ancestor {
            if let table = firstTable(in: current) { return table }
            ancestor = current.superview
        }
        return nil
    }

    private static func firstTable(in view: NSView) -> NSTableView? {
        if let table = view as? NSTableView { return table }
        for subview in view.subviews {
            if let table = firstTable(in: subview) { return table }
        }
        return nil
    }
}
