import Foundation

/// The grid's selection (task 1.8, ADR-0001): the active cell, which has
/// the accent ring (ADR-0002 question 3) and is what the cell inspector
/// shows, and a rectangle of cells from `anchor` to `extent`, which Copy
/// copies.
///
/// A click selects one cell: all three corners are that cell. Shift-click,
/// a drag and Shift with a move key change only `extent`, so the rectangle
/// grows from the active cell, as in a spreadsheet. ⌘A selects every cell
/// and leaves the active cell where it is. A plain value, so it is tested
/// on its own.
struct GridSelection: Equatable, Sendable {
    var active: CellPosition
    var anchor: CellPosition
    var extent: CellPosition
    /// The rectangle runs to the last row of the file, however many rows
    /// that turns out to be: ⌘A, or ⇧⌘↓, while the row count is still an
    /// estimate (DESIGN §3.10 rule 5). Copy then copies to the end.
    var throughLastRow = false
    /// Whole rows, picked by their row numbers (a click, a Shift-click):
    /// Cut deletes them rather than emptying their cells (task 2.6). ⌘A
    /// isn't: it is every cell.
    var wholeRows = false

    /// One cell.
    init(_ cell: CellPosition) {
        active = cell
        anchor = cell
        extent = cell
    }

    init(active: CellPosition, anchor: CellPosition, extent: CellPosition, throughLastRow: Bool = false, wholeRows: Bool = false) {
        self.active = active
        self.anchor = anchor
        self.extent = extent
        self.throughLastRow = throughLastRow
        self.wholeRows = wholeRows
    }

    /// Every cell of a `rows` × `columns` grid, with `active` still active.
    static func all(rows: Int, columns: Int, active: CellPosition) -> GridSelection {
        GridSelection(
            active: active,
            anchor: CellPosition(row: 0, column: 0),
            extent: CellPosition(row: max(0, rows - 1), column: max(0, columns - 1)),
            throughLastRow: true
        )
    }

    /// Row `row`'s every cell (a click on its row number), with the active
    /// cell in column `column`.
    static func row(_ row: Int, columns: Int, column: Int) -> GridSelection {
        GridSelection(
            active: CellPosition(row: row, column: column),
            anchor: CellPosition(row: row, column: 0),
            extent: CellPosition(row: row, column: max(0, columns - 1)),
            wholeRows: true
        )
    }

    var rows: ClosedRange<Int> { min(anchor.row, extent.row)...max(anchor.row, extent.row) }
    var columns: ClosedRange<Int> { min(anchor.column, extent.column)...max(anchor.column, extent.column) }

    /// More than one cell.
    var isRange: Bool { anchor != extent }

    func contains(row: Int, column: Int) -> Bool {
        rows.contains(row) && columns.contains(column)
    }

    /// The rectangle grown (or shrunk) so its moving corner is `cell`.
    /// Whole rows stay whole rows if the corner moves only up or down
    /// (⇧↓ after a click on a row number).
    func extended(to cell: CellPosition, throughLastRow: Bool = false) -> GridSelection {
        GridSelection(
            active: active, anchor: anchor, extent: cell, throughLastRow: throughLastRow,
            wholeRows: wholeRows && cell.column == extent.column
        )
    }

    /// The same cells of the copies of `rows` (Duplicate Row, task 2.5a),
    /// which follow them: each corner's row (within `rows`) moved down by
    /// their count, its column kept. `throughLastRow` is dropped: the
    /// copies end where they end, not at the file's last row.
    func copied(_ rows: ClosedRange<Int>) -> GridSelection {
        func copy(_ cell: CellPosition) -> CellPosition {
            CellPosition(row: min(max(cell.row, rows.lowerBound), rows.upperBound) + rows.count, column: cell.column)
        }
        return GridSelection(active: copy(active), anchor: copy(anchor), extent: copy(extent))
    }

    /// The selection kept inside a grid of `rows` × `columns` (the row
    /// count fell when indexing finished, or the file was read again):
    /// `nil` if the grid is empty.
    func clamped(rows: Int, columns: Int) -> GridSelection? {
        guard rows > 0, columns > 0 else { return nil }
        func clamp(_ cell: CellPosition) -> CellPosition {
            CellPosition(row: min(max(0, cell.row), rows - 1), column: min(max(0, cell.column), columns - 1))
        }
        var extent = clamp(extent)
        if throughLastRow, extent.row >= anchor.row {
            extent.row = rows - 1
        }
        return GridSelection(
            active: clamp(active), anchor: clamp(anchor), extent: extent, throughLastRow: throughLastRow, wholeRows: wholeRows
        )
    }
}
