import AppKit
import CoreText

/// Lays out, off the main thread, the text of the rows about to scroll into
/// view (task 2.0a), and puts the lines in the grid's line cache. Laying
/// out a value not seen before (`CTLineCreateWithAttributedString`) was a
/// quarter of the main thread's work in the slowest scroll frames. The
/// draw then finds each line made, and draws exactly what it would have
/// made itself: the same `LineRequest`, laid out by the same code
/// (`CellPainter.makeCellLine`), cut to the column the same way. A line
/// that isn't ready yet is made by the draw, as before.
///
/// Only rows whose cells are already read are laid out; for the others it
/// asks the data source to read them ahead, and lays them out once they
/// are in.
@MainActor
final class LineReadAhead {
    /// The rows asked for, with the columns asked for in each.
    private var requested: [Int: Range<Int>] = [:]
    /// Bumped by `reset`: lines laid out before it are dropped.
    private var generation = 0
    private var ahead = ScrollAhead()
    /// How many lines it has laid out, for tests.
    private(set) var linesMade = 0

    static let queue = DispatchQueue(label: "io.github.robhaswell.leal.line-read-ahead", qos: .userInitiated)

    /// Forgets everything asked for: the values, the columns or the
    /// colours changed.
    func reset() {
        requested.removeAll()
        generation += 1
        ahead.reset()
    }

    /// After a draw: lays out the rows ahead of `visible` (`ScrollAhead`).
    func update(
        visible: CGRect,
        geometry: GridLayout,
        source: any GridDataSource,
        palette: GridPalette,
        lines: TextLineCache
    ) {
        guard geometry.columnCount > 0,
              let (ranges, rowBudget) = ahead.next(visible: visible, rowHeight: geometry.rowHeight, rows: source.rowCount)
        else { return }
        let columns = geometry.columnRange(minX: visible.minX, maxX: visible.maxX)
        guard !columns.isEmpty else { return }
        if requested.count > 4_000 { requested.removeAll() }
        var budget = rowBudget
        let loaded = min(source.loadedRowCount, source.rowCount)
        let numeric = columns.map { source.isNumeric(column: $0) }
        var work: [LineMaker.Item] = []
        for (range, upwards) in ranges {
            let rows: [Int] = upwards ? range.reversed() : Array(range)
            for row in rows where row < loaded && budget > 0 {
                if let done = requested[row], done.lowerBound <= columns.lowerBound, done.upperBound >= columns.upperBound { continue }
                var items: [LineMaker.Item] = []
                var ready = true
                for column in columns {
                    guard let cell = source.cachedCell(row: row, column: column) else {
                        ready = false
                        break
                    }
                    guard case let .text(value, truncated) = cell, !value.isEmpty || truncated else { continue }
                    let width = geometry.widths[column]
                    let request = LineRequest(value: value, truncated: truncated, width: width, number: numeric[column - columns.lowerBound])
                    guard !lines.contains(request.key) else { continue }
                    items.append(LineMaker.Item(request: request, available: width - 2 * GridMetrics.cellPadding))
                }
                guard ready else {
                    // Its cells aren't read yet: read them, and the rest
                    // of the way, and come back to them next time.
                    let rest = upwards ? range.lowerBound..<(row + 1) : row..<range.upperBound
                    source.readAhead(rows: rest, columns: columns)
                    break
                }
                requested[row] = columns
                work += items
                budget -= 1
            }
        }
        guard !work.isEmpty else { return }
        let maker = LineMaker(palette: palette)
        let generation = generation
        let items = work
        Self.queue.async { [weak self, weak lines] in
            let made = items.map { (key: $0.request.key, line: maker.make($0)) }
            Task { @MainActor [weak self, weak lines] in
                guard let self, let lines, generation == self.generation else { return }
                for item in made {
                    lines.insert(item.line, for: item.key)
                }
                linesMade += made.count
            }
        }
    }
}

/// Which rows are about to scroll into view: past the visible area in the
/// direction it moved since the last call, a screen or four frames' travel
/// ahead, whichever is further; both ways, half a screen, if it moved only
/// sideways. At most a screen, or twice a frame's travel, is taken at a
/// time (`budget`, in rows), so a fling's first frame doesn't take it all.
struct ScrollAhead {
    private var lastVisible: CGRect?

    mutating func reset() {
        lastVisible = nil
    }

    /// The rows ahead of `visible`, each range with whether to walk it
    /// upwards (nearest row first), and how many to take; `nil` if
    /// `visible` hasn't changed since the last call.
    mutating func next(visible: CGRect, rowHeight: CGFloat, rows: Int) -> (ranges: [(rows: Range<Int>, upwards: Bool)], budget: Int)? {
        guard visible != lastVisible, visible.height > 0, rowHeight > 0, rows > 0 else { return nil }
        let dy = lastVisible.map { visible.minY - $0.minY } ?? 0
        lastVisible = visible
        func rowRange(_ minY: CGFloat, _ maxY: CGFloat) -> Range<Int> {
            let first = max(0, Int((minY / rowHeight).rounded(.down)))
            let last = min(rows, Int((maxY / rowHeight).rounded(.up)))
            return first < last ? first..<last : 0..<0
        }
        let reach = max(visible.height, 4 * abs(dy))
        let ranges: [(rows: Range<Int>, upwards: Bool)] = if dy > 0 {
            [(rowRange(visible.maxY, visible.maxY + reach), false)]
        } else if dy < 0 {
            [(rowRange(visible.minY - reach, visible.minY), true)]
        } else {
            [
                (rowRange(visible.maxY, visible.maxY + visible.height / 2), false),
                (rowRange(visible.minY - visible.height / 2, visible.minY), true),
            ]
        }
        let budget = max(Int((visible.height / rowHeight).rounded(.up)), 2 * Int((abs(dy) / rowHeight).rounded(.up)) + 2)
        return (ranges, budget)
    }
}

/// Lays out, off the main thread, the gutter's numbers for the rows about
/// to scroll into view (task 2.0a), as `LineReadAhead` does the cells'.
@MainActor
final class NumberReadAhead {
    private var requested: Set<Int> = []
    private var generation = 0
    private var ahead = ScrollAhead()

    func reset() {
        requested.removeAll()
        generation += 1
        ahead.reset()
    }

    /// After a draw: lays out the numbers of the rows ahead of `visible`
    /// that `numbers` hasn't got, in `font` and `color`, for rows up to
    /// `loaded`.
    func update(visible: CGRect, rowHeight: CGFloat, loaded: Int, font: NSFont, color: CGColor, numbers: LineCache<Int>) {
        guard let (ranges, rowBudget) = ahead.next(visible: visible, rowHeight: rowHeight, rows: loaded) else { return }
        if requested.count > 4_000 { requested.removeAll() }
        var budget = rowBudget
        var rows: [Int] = []
        for (range, upwards) in ranges {
            for row in upwards ? Array(range.reversed()) : Array(range) where budget > 0 {
                guard !requested.contains(row), !numbers.contains(row) else { continue }
                requested.insert(row)
                rows.append(row)
                budget -= 1
            }
        }
        guard !rows.isEmpty else { return }
        let maker = NumberMaker(attributes: CellPainter.attributes(font: font, color: color), color: color)
        let generation = generation
        let wanted = rows
        LineReadAhead.queue.async { [weak self, weak numbers] in
            let made = wanted.map { (row: $0, line: maker.make($0)) }
            Task { @MainActor [weak self, weak numbers] in
                guard let self, let numbers, generation == self.generation else { return }
                for item in made {
                    numbers.insert(item.line, for: item.row)
                }
            }
        }
    }
}

/// Lays out row numbers on any thread, as the gutter does.
private struct NumberMaker: @unchecked Sendable {
    // @unchecked: as `LineMaker`.
    let attributes: CFDictionary
    let color: CGColor

    func make(_ row: Int) -> TextLine {
        CellPainter.makeLine(String(row + 1), symbols: [], attributes: attributes, symbolColor: color)
    }
}

/// Lays out cells' lines on any thread, as `CellPainter.makeCellLine` and
/// `CellPainter.drawText` do on the main thread.
struct LineMaker: @unchecked Sendable {
    // @unchecked: it holds Core Text attribute dictionaries (fonts and
    // colours), which are immutable, and Core Text documents its fonts and
    // lines as safe to use from any thread.
    struct Item: Sendable {
        let request: LineRequest
        /// The width the text has in its column.
        let available: CGFloat
    }

    private let cell: CFDictionary
    private let number: CFDictionary
    private let symbolColor: CGColor

    @MainActor
    init(palette: GridPalette) {
        cell = CellPainter.attributes(font: GridFonts.cell, color: palette.text)
        number = CellPainter.attributes(font: GridFonts.number, color: palette.text)
        symbolColor = palette.secondaryText
    }

    func make(_ item: Item) -> TextLine {
        let attributes = item.request.key.number ? number : cell
        let line = CellPainter.makeCellLine(item.request.shown, truncated: item.request.ellipsis, attributes: attributes, symbolColor: symbolColor)
        // Its glyphs' offsets, for find's marks.
        _ = line.offset(at: 0)
        // Cut to the column with an ellipsis, as `CellPainter.drawText`
        // does: in the cell's font and the text colour.
        if item.available > 2, line.width > item.available {
            let ellipsis = CellPainter.makeLine("…", symbols: [], attributes: attributes, symbolColor: symbolColor)
            line.fit(item.available, ellipsis: ellipsis.line)
            line.fittedRuns = line.fitted.flatMap(GlyphRun.runs(of:))
        }
        return line
    }
}
