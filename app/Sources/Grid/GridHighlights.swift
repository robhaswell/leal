import Foundation

/// Where the find bar's query is in one cell (task 1.8, mockup 04a).
struct CellHighlight: Equatable, Sendable {
    /// Where the query is in the cell's value as the core gives it (the
    /// start the grid shows), in UTF-16 units. Empty if the only matches
    /// are further on than the grid shows.
    let ranges: [NSRange]
    /// The current match: drawn stronger, in place of the active cell's
    /// ring.
    let isCurrent: Bool
}

/// What the grid draws over cells besides their values: find's highlights.
/// The grid asks before drawing a region, then for each text cell in it.
/// The find model (`FindModel`) gives them from the core; the grid knows
/// nothing about searching.
@MainActor
protocol GridHighlighter: AnyObject {
    /// Called before a region is drawn, so the highlights can be read in
    /// one go.
    func prepareHighlights(rows: Range<Int>, columns: Range<Int>)
    /// The cell's highlight, if it is a match.
    func highlight(row: Int, column: Int) -> CellHighlight?
}

extension CellText {
    /// `ranges` in `value` (UTF-16 units) as ranges in the text
    /// `display(value)` gives. Every symbol `display` draws is one UTF-16
    /// unit for one in the value, except a CRLF, which is one `↵` for two;
    /// so only values with a CR need their ranges moved.
    static func displayRanges(_ ranges: [NSRange], in value: String) -> [NSRange] {
        guard !ranges.isEmpty, value.utf8.contains(0x0D) else { return ranges }
        // `removed[i]`: units dropped before value unit `i` (the LF of
        // each CRLF before it).
        var removed: [Int] = []
        removed.reserveCapacity(value.utf16.count + 1)
        var dropped = 0
        var previousWasCR = false
        for unit in value.utf16 {
            if unit == 0x0A, previousWasCR {
                removed.append(dropped)
                dropped += 1
            } else {
                removed.append(dropped)
            }
            previousWasCR = unit == 0x0D
        }
        removed.append(dropped)
        func moved(_ offset: Int) -> Int {
            let clamped = min(max(0, offset), removed.count - 1)
            return clamped - removed[clamped]
        }
        return ranges.compactMap { range in
            let start = moved(range.location)
            let end = moved(range.location + range.length)
            return end > start ? NSRange(location: start, length: end - start) : nil
        }
    }
}
