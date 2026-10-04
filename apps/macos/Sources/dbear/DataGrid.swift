import AppKit
import DBKit
import SwiftUI

/// Infinite-scroll hooks for grids backed by a paged table.
struct GridPaging {
    /// More rows exist on the server.
    var hasMore: Bool
    var isLoading: Bool
    var error: String?
    var loadMore: () -> Void
    var retry: () -> Void
}

/// Server-side sorting for grids backed by a table: header clicks call `toggle` with the column name.
struct GridSorting {
    var keys: [SortKey]
    var toggle: (String) -> Void
}

/// Inline editing hooks for grids of an editable table (see `PendingEdits`).
struct GridEditing {
    var edits: PendingEdits
    /// Set by the model to start editing a cell (e.g. a new row's first cell).
    var editRequest: CellAddress?
    var setCell: (_ row: Int, _ column: Int, _ value: EditValue) -> Void
    var addRow: () -> Void
    var deleteRows: (Set<Int>) -> Void
    var revertRows: (Set<Int>) -> Void
    var selectionChanged: (Set<Int>) -> Void
    /// Called once `editRequest` was acted on.
    var requestHandled: () -> Void
}

/// Foreign keys of a table's grid: hovering a key cell shows an arrow that calls `open` with the
/// key and the row's values for its columns (in key order).
struct GridForeignKeys {
    var keys: [ForeignKeyInfo]
    var open: (ForeignKeyInfo, [DBValue]) -> Void
}

/// Result grid used by table tabs (editable when given `editing`) and script results.
///
/// Backed by a plain `NSTableView` rather than SwiftUI's `Table`: cells are reused text fields,
/// so scrolling stays smooth with thousands of rows, and appending a page only tells the table
/// the row count grew instead of diffing every row.
struct DataGrid: View {
    let result: QueryResult
    /// Changes when the data is replaced (reload / re-run), not when pages are appended.
    var version: Int = 0
    var duration: Duration? = nil
    var paging: GridPaging? = nil
    var sorting: GridSorting? = nil
    var editing: GridEditing? = nil
    var foreignKeys: GridForeignKeys? = nil
    /// New rows are loading (re-sort, refresh) while these stay on screen.
    var isReloading = false

    var body: some View {
        GridTable(result: result, version: version, paging: paging, sorting: sorting, editing: editing, foreignKeys: foreignKeys)
            .safeAreaInset(edge: .bottom, spacing: 0) {
                StatusBar(
                    loaded: result.rows.count, total: result.totalCount, truncated: result.truncated,
                    columns: result.columns.count, duration: duration, paging: paging,
                    isReloading: isReloading
                )
            }
    }
}

// MARK: - NSTableView bridge

private struct GridTable: NSViewRepresentable {
    let result: QueryResult
    let version: Int
    let paging: GridPaging?
    let sorting: GridSorting?
    let editing: GridEditing?
    let foreignKeys: GridForeignKeys?

    func makeCoordinator() -> GridData { GridData() }

    func makeNSView(context: Context) -> NSScrollView {
        let table = GridTableView()
        table.style = .inset
        table.usesAlternatingRowBackgroundColors = true
        table.rowHeight = 24
        table.intercellSpacing = NSSize(width: 12, height: 0)
        table.usesAutomaticRowHeights = false
        table.allowsMultipleSelection = true
        table.allowsColumnReordering = true
        table.allowsColumnResizing = true
        // Header clicks sort instead of selecting the column.
        table.allowsColumnSelection = false
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.headerView = NSTableHeaderView()
        table.dataSource = context.coordinator
        table.delegate = context.coordinator

        // Pinned: wide results keep their horizontal scrollbar visible.
        let scroll = PinnedScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.useThinScrollers()
        scroll.drawsBackground = false
        scroll.contentView.postsBoundsChangedNotifications = true

        context.coordinator.attach(table: table, scrollView: scroll)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.update(
            result: result, version: version, paging: paging, sorting: sorting, editing: editing,
            foreignKeys: foreignKeys)
    }

    static func dismantleNSView(_ scroll: NSScrollView, coordinator: GridData) {
        coordinator.detach()
    }
}

/// Data source + delegate. Keeps the rows the table shows (new rows first, then the loaded ones)
/// and runs inline editing.
@MainActor
final class GridData: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate, NSMenuDelegate {
    private weak var table: GridTableView?
    private var columns: [ColumnInfo] = []
    private var allRows: [Row] = []
    private var rows: [Row] = []
    private var editing: GridEditing?
    private var edits = PendingEdits()
    /// The field editor over the cell being edited.
    private var editor: CellEditor?
    /// Column (model index) of the last clicked cell: Return edits that one.
    private var lastColumn: Int?
    private var version = Int.min
    private var paging: GridPaging?
    private var sorting: GridSorting?
    private var observer: NSObjectProtocol?
    private var loadRequested = false
    private var foreignKeys: GridForeignKeys?
    /// Model column index to the foreign key it belongs to (the narrowest one, if several).
    private var keyByColumn: [Int: ForeignKeyInfo] = [:]
    /// The foreign key cell under the mouse (row index, model column), showing `linkButton`.
    private var hoveredLink: (row: Int, column: Int)?
    private lazy var linkButton: LinkButton = {
        let button = LinkButton()
        button.target = self
        button.action = #selector(openHoveredLink)
        return button
    }()

    /// Start fetching the next page this many rows before the end: about a page ahead,
    /// so fast scrolling doesn't run into the end and wait.
    private let prefetchDistance = 500
    private static let cellID = NSUserInterfaceItemIdentifier("cell")
    private static let rowID = NSUserInterfaceItemIdentifier("row")

    func attach(table: GridTableView, scrollView: NSScrollView) {
        self.table = table
        table.target = self
        table.action = #selector(clicked)
        table.doubleAction = #selector(doubleClicked)
        table.onHover = { [weak self] point in self?.hover(at: point) }
        let menu = NSMenu()
        menu.delegate = self
        table.menu = menu
        observer = NotificationCenter.default.addObserver(
            forName: NSView.boundsDidChangeNotification, object: scrollView.contentView, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.prefetchIfNeeded()
                self?.hoverUnderMouse()
            }
        }
    }

    func detach() {
        if let observer { NotificationCenter.default.removeObserver(observer) }
    }

    func update(
        result: QueryResult, version: Int, paging: GridPaging?, sorting: GridSorting?, editing: GridEditing?,
        foreignKeys: GridForeignKeys?
    ) {
        self.paging = paging
        self.sorting = sorting
        self.editing = editing
        let keysChanged = foreignKeys?.keys != self.foreignKeys?.keys
        self.foreignKeys = foreignKeys
        defer {
            if keysChanged || hoveredLink != nil {
                // After the rows below are settled: the hovered cell may be gone or no longer a link.
                DispatchQueue.main.async { [weak self] in self?.hoverUnderMouse() }
            }
        }
        let newEdits = editing?.edits ?? PendingEdits()
        let editsChanged = newEdits != edits
        edits = newEdits
        defer {
            if let table { showSortIndicators(table) }
            if let request = editing?.editRequest {
                // Not during SwiftUI's update: starting an edit changes the model.
                DispatchQueue.main.async { [weak self] in
                    self?.editing?.requestHandled()
                    self?.beginEditing(row: request.row, column: request.column)
                }
            }
        }
        if !(paging?.isLoading ?? false) { loadRequested = false }
        guard let table else { return }
        if (editing == nil) != (table.onReturn == nil) {
            table.onReturn = editing == nil ? nil : { [weak self] in self?.editSelectedRow() }
            table.onDelete = editing == nil ? nil : { [weak self] in self?.deleteSelectedRows() }
        }

        if version != self.version || result.columns != columns {
            finishEditing(commit: false)
            self.version = version
            allRows = result.rows
            if result.columns != columns {
                columns = result.columns
                rebuildColumns(table)
            }
            hideLink()
            rebuildRows()
            table.reloadData()
            table.scrollRowToVisible(0)
        } else if result.rows.count != allRows.count {
            let appended = result.rows.count > allRows.count && !editsChanged
            let selected = selectedIDs()
            allRows = result.rows
            if appended {
                // A new page only extends the table: no reload, no diff, scroll position untouched.
                rebuildRows()
                table.noteNumberOfRowsChanged()
            } else {
                reload(table, keeping: selected)
            }
        } else if editsChanged {
            reload(table, keeping: selectedIDs())
        }
        if keysChanged {
            indexForeignKeys(table)
        }
        // Tall windows can show the whole first page; ask for more on the next run loop turn
        // (not during SwiftUI's view update).
        DispatchQueue.main.async { [weak self] in self?.prefetchIfNeeded() }
    }

    private func rebuildRows() {
        // New rows stay on top; their cells come from `edits`.
        let inserted = edits.inserted.map { Row(id: $0.id, values: Array(repeating: .null, count: columns.count)) }
        rows = inserted + allRows
    }

    private func selectedIDs() -> Set<Int> {
        guard let table else { return [] }
        return Set(table.selectedRowIndexes.compactMap { rows.indices.contains($0) ? rows[$0].id : nil })
    }

    /// Rebuilds the shown rows and reloads; new rows shift the others, so selection follows row ids.
    private func reload(_ table: NSTableView, keeping selected: Set<Int>) {
        rebuildRows()
        table.reloadData()
        let indexes = IndexSet(rows.indices.filter { selected.contains(rows[$0].id) })
        table.selectRowIndexes(indexes, byExtendingSelection: false)
    }

    /// Loads the next page once the last visible row is near the end.
    private func prefetchIfNeeded() {
        guard let table, let paging, paging.hasMore, !paging.isLoading, paging.error == nil, !loadRequested
        else { return }
        let visible = table.rows(in: table.visibleRect)
        guard NSMaxRange(visible) >= rows.count - prefetchDistance else { return }
        loadRequested = true
        paging.loadMore()
    }

    /// Native ▲/▼ in the header of the sorted column (only the primary key of the sort).
    private func showSortIndicators(_ table: NSTableView) {
        let primary = sorting?.keys.first
        var highlighted: NSTableColumn?
        for column in table.tableColumns {
            guard let index = Int(column.identifier.rawValue), columns.indices.contains(index) else { continue }
            let sorted = primary?.column == columns[index].name
            let image = sorted ? NSImage(named: primary!.descending ? "NSDescendingSortIndicator" : "NSAscendingSortIndicator") : nil
            if table.indicatorImage(in: column) !== image { table.setIndicatorImage(image, in: column) }
            if sorted { highlighted = column }
        }
        if table.highlightedTableColumn !== highlighted { table.highlightedTableColumn = highlighted }
    }

    func tableView(_ tableView: NSTableView, didClick tableColumn: NSTableColumn) {
        guard let sorting, let index = Int(tableColumn.identifier.rawValue), columns.indices.contains(index) else { return }
        sorting.toggle(columns[index].name)
    }

    // MARK: Foreign key links

    /// Maps columns to their foreign keys (single-column keys win over composite ones) and notes
    /// the reference in the header tooltips.
    private func indexForeignKeys(_ table: NSTableView) {
        keyByColumn = [:]
        let keys = (foreignKeys?.keys ?? []).sorted { $0.columns.count < $1.columns.count }
        for key in keys {
            for name in key.columns {
                guard let index = columns.firstIndex(where: { $0.name == name }), keyByColumn[index] == nil else { continue }
                keyByColumn[index] = key
            }
        }
        for column in table.tableColumns {
            guard let index = Int(column.identifier.rawValue), columns.indices.contains(index) else { continue }
            column.headerToolTip = headerToolTip(for: index)
        }
    }

    private func headerToolTip(for index: Int) -> String {
        let info = columns[index]
        var lines = [info.typeName.isEmpty ? info.name : "\(info.name) \u{B7} \(info.typeName)"]
        if let key = keyByColumn[index] {
            lines.append("References \(Self.reference(key))")
        }
        if sorting != nil { lines.append("Click to sort") }
        return lines.joined(separator: "\n")
    }

    /// `public.users(id)`.
    private static func reference(_ key: ForeignKeyInfo) -> String {
        let table = key.referencedSchema.isEmpty ? key.referencedTable : "\(key.referencedSchema).\(key.referencedTable)"
        return key.referencedColumns.isEmpty ? table : "\(table)(\(key.referencedColumns.joined(separator: ", ")))"
    }

    /// The foreign key a cell links through and the row's values for the key's columns, or `nil`
    /// when it isn't a link: not a key column, a NULL in the key, a new or deleted row, or an unsaved edit.
    private func link(row: Int, column: Int) -> (key: ForeignKeyInfo, values: [DBValue])? {
        guard foreignKeys != nil, let key = keyByColumn[column], rows.indices.contains(row) else { return nil }
        let id = rows[row].id
        guard id >= 0, !edits.deleted.contains(id) else { return nil }
        var values: [DBValue] = []
        for name in key.columns {
            guard let index = columns.firstIndex(where: { $0.name == name }), !columns[index].isBinary,
                  edits.value(row: id, column: index) == nil,
                  let value = rows[row].values[safe: index], !value.isNull
            else { return nil }
            values.append(value)
        }
        return (key, values)
    }

    /// `point` is in the table's coordinates; `nil` when the mouse left it.
    private func hover(at point: NSPoint?) {
        guard let table, let point, editor == nil else { return hideLink() }
        let row = table.row(at: point)
        let tableColumn = table.column(at: point)
        guard row >= 0, let column = modelColumn(atTableColumn: tableColumn), link(row: row, column: column) != nil else {
            return hideLink()
        }
        if let hovered = hoveredLink, hovered == (row, column), linkButton.superview != nil { return }
        hideLink()
        hoveredLink = (row, column)
        let cellFrame = table.frameOfCell(atColumn: tableColumn, row: row)
        let size = LinkButton.size
        linkButton.frame = NSRect(
            x: cellFrame.maxX - size, y: cellFrame.midY - size / 2, width: size, height: size
        ).integral
        let emphasized = table.selectedRowIndexes.contains(row) && table.window?.firstResponder === table
        linkButton.contentTintColor = emphasized ? .alternateSelectedControlTextColor : .secondaryLabelColor
        if let key = keyByColumn[column] { linkButton.toolTip = "Open in \(Self.reference(key))" }
        table.addSubview(linkButton)
        cell(row: row, column: tableColumn)?.trailingInset = size + 4
    }

    /// Re-checks what's under the mouse (after scrolling or a reload, which move cells under it).
    private func hoverUnderMouse() {
        guard let table, let window = table.window else { return hideLink() }
        let point = table.convert(window.mouseLocationOutsideOfEventStream, from: nil)
        hover(at: table.visibleRect.contains(point) ? point : nil)
    }

    private func hideLink() {
        if let hovered = hoveredLink, let table {
            let tableColumn = table.column(withIdentifier: NSUserInterfaceItemIdentifier(String(hovered.column)))
            if tableColumn >= 0 { cell(row: hovered.row, column: tableColumn)?.trailingInset = 0 }
        }
        hoveredLink = nil
        linkButton.removeFromSuperview()
    }

    private func cell(row: Int, column tableColumn: Int) -> GridCell? {
        guard let table, row < table.numberOfRows, tableColumn >= 0, tableColumn < table.numberOfColumns else { return nil }
        return table.view(atColumn: tableColumn, row: row, makeIfNecessary: false) as? GridCell
    }

    @objc private func openHoveredLink() {
        guard let hovered = hoveredLink else { return }
        openLink(row: hovered.row, column: hovered.column)
    }

    private func openLink(row: Int, column: Int) {
        guard let foreignKeys, let (key, values) = link(row: row, column: column) else { return }
        foreignKeys.open(key, values)
    }

    private func rebuildColumns(_ table: NSTableView) {
        hideLink()
        table.tableColumns.forEach(table.removeTableColumn)
        for (index, info) in columns.enumerated() {
            let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier(String(index)))
            column.title = info.name
            column.minWidth = 40
            column.width = Self.idealWidth(for: info)
            column.headerCell.alignment = info.isNumeric ? .right : .left
            table.addTableColumn(column)
        }
        // Column indexes changed: re-map the foreign keys (this also sets the header tooltips).
        indexForeignKeys(table)
    }

    private static func idealWidth(for column: ColumnInfo) -> CGFloat {
        let type = column.typeName.lowercased()
        return switch type {
        case "boolean", "bool", "smallint", "integer", "int4", "int2": 80
        case "bigint", "int8": 100
        case "uuid": 300
        case let t where t.contains("time"): 220
        default: 160
        }
    }

    // MARK: NSTableViewDataSource / Delegate

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        let view = tableView.makeView(withIdentifier: Self.rowID, owner: nil) as? GridRowView ?? {
            let r = GridRowView()
            r.identifier = Self.rowID
            return r
        }()
        let id = rows.indices.contains(row) ? rows[row].id : 0
        view.mark = id < 0 ? .inserted : edits.deleted.contains(id) ? .deleted : nil
        return view
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let tableColumn, let index = Int(tableColumn.identifier.rawValue),
              rows.indices.contains(row), columns.indices.contains(index)
        else { return nil }
        let cell = tableView.makeView(withIdentifier: Self.cellID, owner: nil) as? GridCell ?? {
            let c = GridCell()
            c.identifier = Self.cellID
            return c
        }()
        let id = rows[row].id
        let isHovered = hoveredLink.map { $0 == (row, index) } ?? false
        cell.trailingInset = isHovered ? LinkButton.size + 4 : 0
        if edits.deleted.contains(id) {
            cell.show(rows[row].values[safe: index] ?? .null, mark: .deleted)
        } else if let edit = edits.value(row: id, column: index) {
            cell.show(edit: edit, mark: id < 0 ? .inserted : .edited)
        } else {
            cell.show(rows[row].values[safe: index] ?? .null)
        }
        return cell
    }

    func tableViewSelectionDidChange(_ notification: Notification) {
        guard let editing, let table else { return }
        editing.selectionChanged(Set(table.selectedRowIndexes.compactMap { rows.indices.contains($0) ? rows[$0].id : nil }))
    }

    // MARK: Editing

    private func isEditable(column: Int) -> Bool {
        editing != nil && columns.indices.contains(column) && !columns[column].isBinary
    }

    /// Model column index of a table column (columns can be reordered by dragging).
    private func modelColumn(atTableColumn index: Int) -> Int? {
        guard let table, table.tableColumns.indices.contains(index) else { return nil }
        return Int(table.tableColumns[index].identifier.rawValue)
    }

    @objc private func clicked() {
        guard let table, table.clickedColumn >= 0 else { return }
        lastColumn = modelColumn(atTableColumn: table.clickedColumn)
    }

    @objc private func doubleClicked() {
        guard let table, rows.indices.contains(table.clickedRow), let column = modelColumn(atTableColumn: table.clickedColumn)
        else { return }
        beginEditing(row: rows[table.clickedRow].id, column: column)
    }

    /// Return: edit the last clicked column of the selected row (or its first editable cell).
    private func editSelectedRow() {
        guard let table, rows.indices.contains(table.selectedRow) else { return }
        let fallback = displayOrder().first { isEditable(column: $0) }
        guard let column = lastColumn.flatMap({ isEditable(column: $0) ? $0 : nil }) ?? fallback else { return }
        beginEditing(row: rows[table.selectedRow].id, column: column)
    }

    private func deleteSelectedRows() {
        guard let editing, let table else { return }
        let ids = Set(table.selectedRowIndexes.compactMap { rows.indices.contains($0) ? rows[$0].id : nil })
        editing.deleteRows(ids)
    }

    /// Model column indexes in on-screen order.
    private func displayOrder() -> [Int] {
        table?.tableColumns.compactMap { Int($0.identifier.rawValue) } ?? []
    }

    /// Opens a field editor over a cell. Its text is the cell's current value (blank for NULL/DEFAULT).
    func beginEditing(row id: Int, column: Int) {
        guard let table, isEditable(column: column), !edits.deleted.contains(id),
              let rowIndex = rows.firstIndex(where: { $0.id == id })
        else { return }
        hideLink()
        let tableColumn = table.column(withIdentifier: NSUserInterfaceItemIdentifier(String(column)))
        guard tableColumn >= 0 else { return }
        finishEditing(commit: true)

        table.selectRowIndexes(IndexSet(integer: rowIndex), byExtendingSelection: false)
        table.scrollRowToVisible(rowIndex)
        table.scrollColumnToVisible(tableColumn)
        lastColumn = column

        let current: EditValue = edits.value(row: id, column: column) ?? {
            let original = rows[rowIndex].values[safe: column] ?? .null
            return original.isNull ? .null : .text(original.displayString)
        }()
        let field = CellEditor(frame: table.frameOfCell(atColumn: tableColumn, row: rowIndex).insetBy(dx: -CellEditor.inset, dy: 0))
        field.row = id
        field.column = column
        field.font = GridCell.font
        field.alignment = columns[column].isNumeric ? .right : .left
        switch current {
        case .text(let text): field.stringValue = text
        case .null: field.placeholderString = "NULL"
        case .default: field.placeholderString = "DEFAULT"
        }
        field.startedBlank = field.stringValue.isEmpty
        field.delegate = self
        table.addSubview(field)
        editor = field
        table.window?.makeFirstResponder(field)
        field.currentEditor()?.selectAll(nil)
    }

    /// Ends the current edit. `move`: then edit the next (+1) or previous (-1) editable cell in the row.
    private func finishEditing(commit: Bool, move: Int = 0) {
        guard let field = editor else { return }
        editor = nil
        let (id, column) = (field.row, field.column)
        if commit {
            let text = field.stringValue
            // Leaving a NULL/DEFAULT cell blank keeps it as it was.
            if !(text.isEmpty && field.startedBlank) {
                editing?.setCell(id, column, .text(text))
            }
        }
        field.removeFromSuperview()
        if let table, table.window?.firstResponder == nil || table.window?.firstResponder is NSText {
            table.window?.makeFirstResponder(table)
        }
        guard move != 0 else { return }
        let order = displayOrder().filter { isEditable(column: $0) }
        guard let position = order.firstIndex(of: column), order.indices.contains(position + move) else { return }
        let next = order[position + move]
        // After the model update above has reached the grid.
        DispatchQueue.main.async { [weak self] in self?.beginEditing(row: id, column: next) }
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)): finishEditing(commit: true)
        case #selector(NSResponder.insertTab(_:)): finishEditing(commit: true, move: 1)
        case #selector(NSResponder.insertBacktab(_:)): finishEditing(commit: true, move: -1)
        case #selector(NSResponder.cancelOperation(_:)): finishEditing(commit: false)
        default: return false
        }
        return true
    }

    /// Clicking elsewhere ends editing and keeps what was typed.
    func controlTextDidEndEditing(_ notification: Notification) {
        if (notification.object as? CellEditor) === editor { finishEditing(commit: true) }
    }

    // MARK: Context menu

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        guard let table, rows.indices.contains(table.clickedRow) else { return }
        let row = rows[table.clickedRow]
        let column = modelColumn(atTableColumn: table.clickedColumn)
        // Acting on the selection when the click is inside it, like Finder.
        let targets: Set<Int> = table.selectedRowIndexes.contains(table.clickedRow)
            ? Set(table.selectedRowIndexes.compactMap { rows.indices.contains($0) ? rows[$0].id : nil })
            : [row.id]

        if let editing {
            if let column, isEditable(column: column), !edits.deleted.contains(row.id) {
                menu.addItem(ClosureMenuItem("Edit Cell") { [weak self] in self?.beginEditing(row: row.id, column: column) })
                if columns[column].isNullable {
                    menu.addItem(ClosureMenuItem("Set to NULL") {
                        for id in targets { editing.setCell(id, column, .null) }
                    })
                }
                menu.addItem(ClosureMenuItem("Set to Default") {
                    for id in targets { editing.setCell(id, column, .default) }
                })
                menu.addItem(.separator())
            }
            menu.addItem(ClosureMenuItem("Add Row") { editing.addRow() })
            let count = targets.count
            menu.addItem(ClosureMenuItem(count == 1 ? "Delete Row" : "Delete \(count) Rows") { editing.deleteRows(targets) })
            if targets.contains(where: { $0 < 0 || edits.updates[$0] != nil || edits.deleted.contains($0) }) {
                menu.addItem(ClosureMenuItem(count == 1 ? "Revert Row" : "Revert \(count) Rows") { editing.revertRows(targets) })
            }
            menu.addItem(.separator())
        }
        if let column, link(row: table.clickedRow, column: column) != nil, let key = keyByColumn[column] {
            let clickedRow = table.clickedRow
            menu.addItem(ClosureMenuItem("Open \(key.referencedTable) Row") { [weak self] in
                self?.openLink(row: clickedRow, column: column)
            })
            menu.addItem(.separator())
        }
        if let column {
            let value = edits.value(row: row.id, column: column).map { edit -> String in
                switch edit {
                case .text(let t): t
                case .null: "NULL"
                case .default: "DEFAULT"
                }
            } ?? (row.values[safe: column]?.displayString ?? "")
            menu.addItem(ClosureMenuItem("Copy Value") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(value, forType: .string)
            })
        }
    }
}

// MARK: - Cells

/// How a row or cell differs from what's saved.
enum EditMark {
    case edited, inserted, deleted

    var tint: NSColor {
        switch self {
        case .edited: .systemOrange.withAlphaComponent(0.22)
        case .inserted: .systemGreen.withAlphaComponent(0.16)
        case .deleted: .systemRed.withAlphaComponent(0.16)
        }
    }
}

/// Row container that flattens its cells into a single layer: one texture per row instead of
/// one per cell, which is what keeps fast scrolling cheap. New and deleted rows get a tint.
private final class GridRowView: NSTableRowView {
    var mark: EditMark? {
        didSet { if mark != oldValue { needsDisplay = true } }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        canDrawSubviewsIntoLayer = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func drawBackground(in dirtyRect: NSRect) {
        super.drawBackground(in: dirtyRect)
        guard let mark else { return }
        mark.tint.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 10, dy: 0), xRadius: 4, yRadius: 4).fill()
    }
}

/// The grid's table: Return edits the selected row, ⌫ deletes it.
final class GridTableView: NSTableView {
    var onReturn: (() -> Void)?
    var onDelete: (() -> Void)?
    /// Mouse position in the table's coordinates (`nil` when it leaves), for the foreign key links.
    var onHover: ((NSPoint?) -> Void)?
    private var hoverArea: NSTrackingArea?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let hoverArea { removeTrackingArea(hoverArea) }
        let area = NSTrackingArea(
            rect: .zero, options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect],
            owner: self, userInfo: nil)
        addTrackingArea(area)
        hoverArea = area
    }

    override func mouseMoved(with event: NSEvent) {
        super.mouseMoved(with: event)
        onHover?(convert(event.locationInWindow, from: nil))
    }

    override func mouseExited(with event: NSEvent) {
        super.mouseExited(with: event)
        onHover?(nil)
    }

    override func keyDown(with event: NSEvent) {
        let plain = event.modifierFlags.intersection([.command, .option, .control, .shift]).isEmpty
        let isReturn = event.keyCode == 36 || event.keyCode == 76  // Return, Enter
        let isDelete = event.keyCode == 51 || event.keyCode == 117  // ⌫, ⌦
        if plain, isReturn, let onReturn {
            onReturn()
        } else if plain, isDelete, let onDelete {
            onDelete()
        } else {
            super.keyDown(with: event)
        }
    }
}

/// Field editor placed over a cell while it's edited.
final class CellEditor: NSTextField {
    var row = 0
    var column = 0
    /// The cell was NULL/DEFAULT: leaving it blank changes nothing.
    var startedBlank = false

    /// Horizontal room around the text, so it sits where `GridCell` drew it.
    static let inset: CGFloat = 4

    override class var cellClass: AnyClass? {
        get { CellEditorCell.self }
        set {}
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        isBordered = false
        drawsBackground = true
        backgroundColor = .textBackgroundColor
        focusRingType = .exterior
        usesSingleLineMode = true
        lineBreakMode = .byClipping
        cell?.isScrollable = true
        cell?.wraps = false
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// Lays out the editor's text exactly like `GridCell.draw`: inset horizontally, one line centered vertically.
private final class CellEditorCell: NSTextFieldCell {
    /// Own field editor without line fragment padding, so the text isn't nudged sideways.
    private lazy var editor: NSTextView = {
        let view = NSTextView()
        view.isFieldEditor = true
        view.textContainer?.lineFragmentPadding = 0
        return view
    }()
    private var adjusted = false

    override func fieldEditor(for controlView: NSView) -> NSTextView? { editor }

    override func drawingRect(forBounds rect: NSRect) -> NSRect {
        if adjusted { return rect }
        let font = font ?? GridCell.font
        let lineHeight = ceil(font.ascender - font.descender + font.leading)
        return NSRect(x: rect.minX + CellEditor.inset, y: rect.minY + ((rect.height - lineHeight) / 2).rounded(),
                      width: rect.width - 2 * CellEditor.inset, height: lineHeight)
    }

    override func titleRect(forBounds rect: NSRect) -> NSRect { drawingRect(forBounds: rect) }

    override func edit(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, event: NSEvent?) {
        let frame = drawingRect(forBounds: rect)
        adjusted = true
        super.edit(withFrame: frame, in: controlView, editor: textObj, delegate: delegate, event: event)
        adjusted = false
    }

    override func select(withFrame rect: NSRect, in controlView: NSView, editor textObj: NSText, delegate: Any?, start selStart: Int, length selLength: Int) {
        let frame = drawingRect(forBounds: rect)
        adjusted = true
        super.select(withFrame: frame, in: controlView, editor: textObj, delegate: delegate, start: selStart, length: selLength)
        adjusted = false
    }
}

/// The arrow at the end of a hovered foreign key cell: opens the row it points at.
private final class LinkButton: NSButton {
    static let size: CGFloat = 16

    override init(frame: NSRect) {
        super.init(frame: frame)
        isBordered = false
        imagePosition = .imageOnly
        image = NSImage(systemSymbolName: "arrow.right.circle.fill", accessibilityDescription: "Open referenced row")?
            .withSymbolConfiguration(.init(pointSize: 13, weight: .regular))
        setButtonType(.momentaryChange)
        refusesFirstResponder = true
    }

    required init?(coder: NSCoder) { fatalError() }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .pointingHand)
    }
}

/// Menu item that runs a closure.
private final class ClosureMenuItem: NSMenuItem {
    private let handler: () -> Void

    init(_ title: String, handler: @escaping () -> Void) {
        self.handler = handler
        super.init(title: title, action: #selector(run), keyEquivalent: "")
        target = self
    }

    required init(coder: NSCoder) { fatalError() }

    @objc private func run() { handler() }
}

/// A cell that draws its text directly: no text field, no Auto Layout, no extra layer.
private final class GridCell: NSTableCellView {
    private var text = ""
    private var style = Style.text
    private var mark: EditMark?
    /// Room kept free at the trailing edge (for the foreign key arrow while hovered).
    var trailingInset: CGFloat = 0 {
        didSet { if trailingInset != oldValue { needsDisplay = true } }
    }

    enum Style { case text, number, null, dimmed }

    static let font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
    private static let nullFont = NSFontManager.shared.convert(font, toHaveTrait: .italicFontMask)
    /// Long values (JSON, text blobs) are cut before reaching text layout.
    private static let maxChars = 512
    private static let left: NSParagraphStyle = paragraph(.left)
    private static let right: NSParagraphStyle = paragraph(.right)

    private static func paragraph(_ alignment: NSTextAlignment) -> NSParagraphStyle {
        let p = NSMutableParagraphStyle()
        p.alignment = alignment
        p.lineBreakMode = .byTruncatingTail
        return p
    }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    override init(frame: NSRect) {
        super.init(frame: frame)
        // The edited-cell tint reaches a little into the gap between columns.
        clipsToBounds = false
    }

    required init?(coder: NSCoder) { fatalError() }
    override var backgroundStyle: NSView.BackgroundStyle {
        didSet { if backgroundStyle != oldValue { needsDisplay = true } }
    }

    /// A loaded value; `mark` is `.deleted` for rows about to be deleted.
    func show(_ value: DBValue, mark: EditMark? = nil) {
        let (text, style): (String, Style) = switch value {
        case .null: ("NULL", .null)
        case .int, .double, .decimal: (value.displayString, .number)
        case .bool(let b): (value.displayString, b ? .text : .dimmed)
        case .text(let s): (Self.singleLine(s), .text)
        }
        set(text, style, mark)
    }

    /// An unsaved value: tinted when it's a changed cell of a loaded row.
    func show(edit: EditValue, mark: EditMark) {
        switch edit {
        case .null: set("NULL", .null, mark)
        case .default: set("DEFAULT", .null, mark)
        case .text(let s): set(Self.singleLine(s), .text, mark)
        }
    }

    private func set(_ text: String, _ style: Style, _ mark: EditMark?) {
        guard text != self.text || style != self.style || mark != self.mark else { return }
        self.text = text
        self.style = style
        self.mark = mark
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        let selected = backgroundStyle == .emphasized
        let color: NSColor = switch style {
        case _ where selected: .alternateSelectedControlTextColor
        case .null: .tertiaryLabelColor
        case .dimmed: .secondaryLabelColor
        case .text, .number: .labelColor
        }
        // Only changed cells are tinted here; new and deleted rows are tinted by the row view.
        if mark == .edited {
            mark!.tint.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: -4, dy: 2), xRadius: 4, yRadius: 4).fill()
        }
        let font = style == .null ? Self.nullFont : Self.font
        let lineHeight = ceil(font.ascender - font.descender + font.leading)
        let rect = NSRect(x: 0, y: ((bounds.height - lineHeight) / 2).rounded(), width: max(0, bounds.width - trailingInset), height: lineHeight)
        var attributes: [NSAttributedString.Key: Any] = [
            .font: font, .foregroundColor: mark == .deleted && !selected ? NSColor.secondaryLabelColor : color,
            .paragraphStyle: style == .number ? Self.right : Self.left,
        ]
        if mark == .deleted { attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
        (text as NSString).draw(with: rect, options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine], attributes: attributes)
    }

    private static func singleLine(_ s: String) -> String {
        let clipped = s.count > maxChars ? String(s.prefix(maxChars)) + "…" : s
        guard clipped.contains(where: \.isNewline) else { return clipped }
        return clipped.replacingOccurrences(of: "\r\n", with: " ↵ ").replacingOccurrences(of: "\n", with: " ↵ ")
    }
}

extension ColumnInfo {
    var isNumeric: Bool {
        let t = typeName.lowercased()
        return ["int", "numeric", "decimal", "real", "double", "float", "serial", "money", "oid"].contains { t.contains($0) }
            && !t.hasSuffix("[]")
    }
}

// MARK: - Status bar

private struct StatusBar: View {
    let loaded: Int
    let total: Int?
    let truncated: Bool
    let columns: Int
    let duration: Duration?
    let paging: GridPaging?
    var isReloading = false

    var body: some View {
        BottomBar {
            Text(rowsText)
            if truncated {
                Image(systemName: "info.circle")
                    .help("Scripts keep the first \(loaded.formatted()) rows. Add a LIMIT or open the table to page through everything.")
            }
            if isReloading {
                ProgressView().controlSize(.mini)
            } else if let paging {
                if paging.isLoading {
                    ProgressView().controlSize(.mini)
                    Text("Loading more…")
                } else if let error = paging.error {
                    Text("Couldn’t load more rows").foregroundStyle(.red).help(error)
                    Button("Retry", action: paging.retry).buttonStyle(.link)
                }
            }
            Spacer()
            if let duration {
                Text(duration.formatted(.units(allowed: [.seconds, .milliseconds], width: .narrow)))
                Text("·")
            }
            Text("\(columns) columns")
        }
    }

    private var rowsText: String {
        if truncated, let total {
            return "First \(loaded.formatted()) of \(total.formatted()) rows"
        }
        let hasMore = paging?.hasMore ?? false
        // Big tables report a planner estimate, which can be below what's already loaded.
        if let total, hasMore || total > loaded {
            return "\(loaded.formatted()) of \(max(total, loaded).formatted()) rows"
        }
        return hasMore ? "\(loaded.formatted())+ rows" : loaded == 1 ? "1 row" : "\(loaded.formatted()) rows"
    }
}

/// The strip under a grid or the structure view: secondary text on the bar material.
struct BottomBar<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        HStack(spacing: 6) { content }
            .font(.callout)
            .foregroundStyle(.secondary)
            .monospacedDigit()
            .padding(.horizontal, 12)
            .frame(height: 30)
            .background(.bar)
            .overlay(alignment: .top) { Divider() }
    }
}
