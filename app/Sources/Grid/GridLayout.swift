import CoreGraphics

/// The grid's sizes, from the approved mockups (ADR-0002): 13 pt text in
/// 22 pt rows, a 26 pt header, columns at most 260 pt wide.
enum GridMetrics {
    static let rowHeight: CGFloat = 22
    static let headerHeight: CGFloat = 26
    /// Space between a column's edge and its text.
    static let cellPadding: CGFloat = 8
    /// The narrowest a column is sized automatically.
    static let minimumColumnWidth: CGFloat = 40
    /// The narrowest a column can be dragged.
    static let resizeMinimumWidth: CGFloat = 24
    /// The widest a column is sized automatically (ADR-0002 question 2).
    /// Wider text is cut with an ellipsis; the user can widen the column.
    static let maximumColumnWidth: CGFloat = 260
    /// The widest a double-click on a column's edge makes it.
    static let fitMaximumWidth: CGFloat = 1000
    /// A column's width before anything has been measured.
    static let defaultColumnWidth: CGFloat = 100
    /// The narrowest the row-number gutter is.
    static let gutterMinimumWidth: CGFloat = 44
    /// How close to a column's edge the pointer must be to resize it.
    static let resizeTolerance: CGFloat = 4
    /// The most characters of a cell the grid asks the core for: more than
    /// fit in a 380 pt column (no character is narrower than about 3 pt).
    /// In a wider column, a longer value ends with an ellipsis; the cell
    /// inspector (1.8) shows all of it. Fewer characters keep each tile's
    /// read from the core small.
    static let maxCellCharacters: UInt32 = 128
}

/// Where rows and columns are: column geometry held as running totals
/// (ADR-0001), and the arithmetic between points and cells. A plain value,
/// so it is tested on its own (`GridLayoutTests`).
///
/// Rows are all `rowHeight` tall, so row `r` starts at `r × rowHeight`.
/// That is exact in `CGFloat` (a `Double`) far beyond 40M rows (880M
/// points), the largest file Leal opens.
struct GridLayout: Equatable {
    let rowHeight: CGFloat
    private(set) var widths: [CGFloat]
    /// `offsets[c]` is the left edge of column `c`; `offsets[count]` is the
    /// total width.
    private(set) var offsets: [CGFloat]

    init(widths: [CGFloat] = [], rowHeight: CGFloat = GridMetrics.rowHeight) {
        self.rowHeight = rowHeight
        self.widths = widths
        offsets = Self.runningTotals(widths, from: 0, offsets: [0])
    }

    var columnCount: Int { widths.count }

    var totalWidth: CGFloat { offsets[widths.count] }

    /// The height of `rows` rows.
    func height(rows: Int) -> CGFloat {
        CGFloat(max(0, rows)) * rowHeight
    }

    /// The rectangle of a cell, in the grid's flipped coordinates.
    func cellRect(row: Int, column: Int) -> CGRect {
        CGRect(x: offsets[column], y: CGFloat(row) * rowHeight, width: widths[column], height: rowHeight)
    }

    /// The rows from `rows` that lie at least partly between `minY` and
    /// `maxY`.
    func rowRange(minY: CGFloat, maxY: CGFloat, rows: Int) -> Range<Int> {
        guard rows > 0, maxY > minY else { return 0..<0 }
        let first = max(0, Int((minY / rowHeight).rounded(.down)))
        let last = min(rows, Int((maxY / rowHeight).rounded(.up)))
        return first < last ? first..<last : 0..<0
    }

    /// The columns that lie at least partly between `minX` and `maxX`.
    func columnRange(minX: CGFloat, maxX: CGFloat) -> Range<Int> {
        guard !widths.isEmpty, maxX > minX, maxX > 0, minX < totalWidth else { return 0..<0 }
        // The first column whose right edge is past minX, and the first
        // whose left edge is at or past maxX.
        let first = firstIndex(where: { offsets[$0 + 1] > minX })
        let end = firstIndex(where: { offsets[$0] >= maxX })
        return first < end ? first..<end : 0..<0
    }

    /// The row at `y`, if it is one of `rows`.
    func row(atY y: CGFloat, rows: Int) -> Int? {
        guard y >= 0 else { return nil }
        let row = Int((y / rowHeight).rounded(.down))
        return row < rows ? row : nil
    }

    /// The column at `x`, if there is one.
    func column(atX x: CGFloat) -> Int? {
        guard x >= 0, x < totalWidth else { return nil }
        return firstIndex(where: { offsets[$0 + 1] > x })
    }

    /// The column whose right edge is within `tolerance` of `x`, for
    /// resizing by dragging the header edge. Ties go to the column on the
    /// left, so a zero-width column can still be widened.
    func columnEdge(nearX x: CGFloat, tolerance: CGFloat = GridMetrics.resizeTolerance) -> Int? {
        guard !widths.isEmpty else { return nil }
        // The first edge at or past x - tolerance.
        let column = firstIndex(where: { offsets[$0 + 1] >= x - tolerance })
        guard column < widths.count, abs(offsets[column + 1] - x) <= tolerance else { return nil }
        return column
    }

    /// Sets one column's width. The running totals after it move.
    mutating func setWidth(_ width: CGFloat, ofColumn column: Int) {
        guard widths.indices.contains(column) else { return }
        widths[column] = max(0, width)
        offsets = Self.runningTotals(widths, from: column, offsets: offsets)
    }

    /// Sets every width.
    mutating func setWidths(_ newWidths: [CGFloat]) {
        widths = newWidths.map { max(0, $0) }
        offsets = Self.runningTotals(widths, from: 0, offsets: [0])
    }

    /// The smallest index in `0..<columnCount` for which `isPast` holds,
    /// or `columnCount`. `isPast` must be false, then true.
    private func firstIndex(where isPast: (Int) -> Bool) -> Int {
        var low = 0
        var high = widths.count
        while low < high {
            let middle = (low + high) / 2
            if isPast(middle) {
                high = middle
            } else {
                low = middle + 1
            }
        }
        return low
    }

    /// `offsets` with the totals from `column` on recomputed.
    private static func runningTotals(_ widths: [CGFloat], from column: Int, offsets: [CGFloat]) -> [CGFloat] {
        var result = Array(offsets.prefix(column + 1))
        if result.isEmpty { result = [0] }
        result.reserveCapacity(widths.count + 1)
        for index in column..<widths.count {
            result.append(result[index] + widths[index])
        }
        return result
    }
}
