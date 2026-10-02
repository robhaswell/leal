import XCTest

@testable import Leal

/// The grid's geometry and logic, with no window: column running totals,
/// the visible range, column sizing, keyboard moves and the tile cache.
@MainActor
final class GridLayoutTests: XCTestCase {
    func testColumnsAreRunningTotals() {
        var layout = GridLayout(widths: [80, 120, 60])
        XCTAssertEqual(layout.offsets, [0, 80, 200, 260])
        XCTAssertEqual(layout.totalWidth, 260)
        layout.setWidth(100, ofColumn: 1)
        XCTAssertEqual(layout.offsets, [0, 80, 180, 240])
        layout.setWidth(-5, ofColumn: 0)
        XCTAssertEqual(layout.offsets, [0, 0, 100, 160])
        layout.setWidth(10, ofColumn: 9)
        XCTAssertEqual(layout.columnCount, 3)
        layout.setWidths([10, 20])
        XCTAssertEqual(layout.offsets, [0, 10, 30])
        XCTAssertEqual(GridLayout().totalWidth, 0)
    }

    func testCellRects() {
        let layout = GridLayout(widths: [80, 120, 60], rowHeight: 22)
        XCTAssertEqual(layout.cellRect(row: 3, column: 1), CGRect(x: 80, y: 66, width: 120, height: 22))
        XCTAssertEqual(layout.height(rows: 1_000_000), 22_000_000)
        XCTAssertEqual(layout.height(rows: -1), 0)
    }

    func testVisibleColumns() {
        let layout = GridLayout(widths: [80, 120, 60])
        XCTAssertEqual(layout.columnRange(minX: 0, maxX: 80), 0..<1)
        XCTAssertEqual(layout.columnRange(minX: 0, maxX: 80.5), 0..<2)
        XCTAssertEqual(layout.columnRange(minX: 79, maxX: 201), 0..<3)
        XCTAssertEqual(layout.columnRange(minX: 80, maxX: 200), 1..<2)
        XCTAssertEqual(layout.columnRange(minX: 250, maxX: 1000), 2..<3)
        XCTAssertEqual(layout.columnRange(minX: 260, maxX: 1000), 0..<0)
        XCTAssertEqual(layout.columnRange(minX: -50, maxX: 0), 0..<0)
        XCTAssertEqual(layout.columnRange(minX: 100, maxX: 100), 0..<0)
        XCTAssertEqual(GridLayout().columnRange(minX: 0, maxX: 100), 0..<0)
    }

    /// Many columns: the binary search agrees with a linear one.
    func testVisibleColumnsAgreeWithALinearSearch() {
        let widths = (0..<200).map { CGFloat(24 + ($0 * 37) % 240) }
        let layout = GridLayout(widths: widths)
        for minX in stride(from: CGFloat(-10), to: layout.totalWidth + 10, by: 97.5) {
            let maxX = minX + 1137
            let expected = (0..<200).filter { layout.offsets[$0 + 1] > minX && layout.offsets[$0] < maxX }
            let range = layout.columnRange(minX: minX, maxX: maxX)
            XCTAssertEqual(Array(range), expected, "\(minX)")
        }
    }

    func testVisibleRows() {
        let layout = GridLayout(widths: [10], rowHeight: 22)
        XCTAssertEqual(layout.rowRange(minY: 0, maxY: 22, rows: 100), 0..<1)
        XCTAssertEqual(layout.rowRange(minY: 0, maxY: 23, rows: 100), 0..<2)
        XCTAssertEqual(layout.rowRange(minY: 21, maxY: 44, rows: 100), 0..<2)
        XCTAssertEqual(layout.rowRange(minY: 2190, maxY: 3000, rows: 100), 99..<100)
        XCTAssertEqual(layout.rowRange(minY: 2200, maxY: 3000, rows: 100), 0..<0)
        XCTAssertEqual(layout.rowRange(minY: 0, maxY: 100, rows: 0), 0..<0)
        XCTAssertEqual(layout.rowRange(minY: -40, maxY: 10, rows: 5), 0..<1)
        // The last rows of a 40M-row file, 880M points down.
        let rows = 40_000_000
        let maxY = layout.height(rows: rows)
        XCTAssertEqual(layout.rowRange(minY: maxY - 440, maxY: maxY, rows: rows), (rows - 20)..<rows)
        XCTAssertEqual(layout.row(atY: maxY - 1, rows: rows), rows - 1)
        XCTAssertNil(layout.row(atY: maxY, rows: rows))
        XCTAssertNil(layout.row(atY: -1, rows: rows))
    }

    func testHitTesting() {
        let layout = GridLayout(widths: [80, 120, 60])
        XCTAssertEqual(layout.column(atX: 0), 0)
        XCTAssertEqual(layout.column(atX: 79.9), 0)
        XCTAssertEqual(layout.column(atX: 80), 1)
        XCTAssertEqual(layout.column(atX: 259), 2)
        XCTAssertNil(layout.column(atX: 260))
        XCTAssertNil(layout.column(atX: -1))
        XCTAssertEqual(layout.columnEdge(nearX: 82), 0)
        XCTAssertEqual(layout.columnEdge(nearX: 198), 1)
        XCTAssertEqual(layout.columnEdge(nearX: 263), 2)
        XCTAssertNil(layout.columnEdge(nearX: 140))
        XCTAssertNil(layout.columnEdge(nearX: 265))
        // A zero-width column can still be widened: the left one wins.
        XCTAssertEqual(GridLayout(widths: [80, 0, 60]).columnEdge(nearX: 80), 0)
    }

    // MARK: Column sizing

    /// Width = characters × 7 (cells) or × 8 (titles), to keep the numbers
    /// easy.
    private func widths(
        columns: Int,
        header: [String],
        rows: [[String]],
        maximum: CGFloat = GridMetrics.maximumColumnWidth
    ) -> [CGFloat] {
        ColumnSizer.widths(
            columns: columns,
            header: header,
            rows: rows.map { $0.map { (text: $0, truncated: false) } },
            maximum: maximum,
            measureCell: { _, text, truncated in CGFloat(text.count + (truncated ? 1 : 0)) * 7 },
            measureHeader: { CGFloat($0.count) * 8 }
        )
    }

    func testColumnsFitTheirWidestCellOrTitle() {
        let padding = 2 * GridMetrics.cellPadding
        let result = widths(
            columns: 4,
            header: ["id", "a long title"],
            rows: [["A-1", "x", "12345678901234567890", ""], ["A-100231", "y"]]
        )
        XCTAssertEqual(result[0], 8 * 7 + padding)
        XCTAssertEqual(result[1], 12 * 8 + padding)
        XCTAssertEqual(result[2], 20 * 7 + padding)
        // Empty, or short: the minimum.
        XCTAssertEqual(result[3], GridMetrics.minimumColumnWidth)
    }

    func testColumnsStopAtTheMaximum() {
        let long = String(repeating: "w", count: 200)
        XCTAssertEqual(widths(columns: 1, header: [], rows: [[long]]), [GridMetrics.maximumColumnWidth])
        XCTAssertEqual(widths(columns: 1, header: [long], rows: []), [GridMetrics.maximumColumnWidth])
        XCTAssertEqual(widths(columns: 1, header: [], rows: [[long]], maximum: 1000), [1000])
        XCTAssertEqual(widths(columns: 0, header: ["a"], rows: [["a"]]), [])
    }

    func testTruncatedCellsCountTheirEllipsis() {
        let result = ColumnSizer.widths(
            columns: 1,
            header: [],
            rows: [[(text: "", truncated: true)]],
            measureCell: { _, _, truncated in truncated ? 100 : 0 },
            measureHeader: { _ in 0 }
        )
        XCTAssertEqual(result, [100 + 2 * GridMetrics.cellPadding])
    }

    func testTheMeasurerAgreesWithCoreText() {
        for font in [GridFonts.cell, GridFonts.number, GridFonts.header] {
            assertMeasures(font)
        }
    }

    private func assertMeasures(_ font: NSFont) {
        let measurer = TextMeasurer(font: font)
        for text in ["Sable Optics", "orders@verity.example", "1196.00", "18.2", "1013.3", "Zoë Zürich", "日本語"] {
            let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: [.font: font]))
            let exact = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
            // No kerning, so it may be a little wider than Core Text; it
            // must never be much narrower, or text would be cut.
            let measured = measurer.width(of: text)
            XCTAssertGreaterThanOrEqual(measured, exact - 0.5, text)
            XCTAssertLessThanOrEqual(measured, exact * 1.025 + 1, text)
        }
        XCTAssertEqual(measurer.width(of: ""), 0)
    }

    // MARK: Cell text

    func testControlCharactersShowAsSymbols() {
        XCTAssertEqual(CellText.display("plain").text, "plain")
        XCTAssertEqual(CellText.display("plain").symbols, [])
        XCTAssertEqual(CellText.display("two\nlines").text, "two↵lines")
        XCTAssertEqual(CellText.display("a\r\nb\rc").text, "a↵b↵c")
        XCTAssertEqual(CellText.display("a\0b\tc\u{7F}").text, "a␀b⇥c␡")
        let (text, symbols) = CellText.display("Zoë\nZürich")
        XCTAssertEqual(text, "Zoë↵Zürich")
        XCTAssertEqual(symbols, [NSRange(location: 3, length: 1)])
        XCTAssertEqual((text as NSString).substring(with: symbols[0]), "↵")
        // A non-BMP character before a symbol: UTF-16 offsets.
        XCTAssertEqual(CellText.display("😀\n").symbols, [NSRange(location: 2, length: 1)])
    }

    /// C1 controls (U+0080 to U+009F) are drawn as `⍰`, not at zero width,
    /// so a cell never looks like text a search for it doesn't find
    /// (phase 1 review, fid-3). U+2028 and U+2029 are line breaks.
    func testC1ControlsAndUnicodeLineBreaksShowAsSymbols() {
        XCTAssertEqual(CellText.display("a\u{81}b").text, "a⍰b")
        XCTAssertEqual(CellText.display("a\u{81}b").symbols, [NSRange(location: 1, length: 1)])
        XCTAssertEqual(CellText.display("\u{80}\u{85}\u{9D}\u{9F}").text, "⍰⍰⍰⍰")
        XCTAssertEqual(CellText.display("one\u{2028}two\u{2029}three").text, "one↵two↵three")
        // Characters next to them in UTF-8 are left alone: U+00A0 to
        // U+00BF (C2 A0 to C2 BF) and other E2 80 characters.
        for plain in ["\u{A0}", "£", "°", "©", "\u{2019}", "\u{2026}", "€", "\u{2027}", "\u{202A}", "Zoë"] {
            XCTAssertEqual(CellText.display(plain).text, plain)
            XCTAssertEqual(CellText.display(plain).symbols, [])
        }
        // One unit for one: find's ranges need no moving.
        let value = "x\u{85}marlow"
        XCTAssertEqual(CellText.displayRanges([NSRange(location: 2, length: 6)], in: value), [NSRange(location: 2, length: 6)])
        XCTAssertEqual((CellText.display(value).text as NSString).substring(with: NSRange(location: 2, length: 6)), "marlow")
        // Each is drawn with some width now.
        let measurer = TextMeasurer(font: GridFonts.cell)
        XCTAssertGreaterThan(measurer.cellWidth(of: "a\u{81}b", truncated: false), measurer.width(of: "ab") + 4)
        XCTAssertGreaterThan(measurer.cellWidth(of: "a\u{2028}b", truncated: false), measurer.width(of: "ab") + 4)
    }

    /// The inspector shows the value itself, with its control characters
    /// drawn as visible glyphs rather than at zero width (fid-3).
    func testTheInspectorDrawsControlCharacters() throws {
        func width(_ text: String) throws -> CGFloat {
            let inspector = CellInspectorView(frame: NSRect(x: 0, y: 0, width: 600, height: CellInspectorView.height))
            inspector.textView.string = text
            let layout = try XCTUnwrap(inspector.textView.layoutManager)
            let container = try XCTUnwrap(inspector.textView.textContainer)
            return layout.boundingRect(forGlyphRange: layout.glyphRange(for: container), in: container).width
        }
        let plain = try width("ab")
        for text in ["a\u{81}b", "a\u{9D}b", "a\0b", "a\u{7F}b"] {
            XCTAssertGreaterThan(try width(text), plain + 4, "\(text.unicodeScalars.map(\.value))")
        }
    }

    // MARK: Keyboard moves

    func testMoves() {
        let start = CellPosition(row: 5, column: 2)
        func move(_ move: GridMove, from cell: CellPosition = start) -> CellPosition {
            move.apply(to: cell, rows: 100, columns: 4, pageRows: 20)
        }
        XCTAssertEqual(move(.up), CellPosition(row: 4, column: 2))
        XCTAssertEqual(move(.down), CellPosition(row: 6, column: 2))
        XCTAssertEqual(move(.left), CellPosition(row: 5, column: 1))
        XCTAssertEqual(move(.right), CellPosition(row: 5, column: 3))
        XCTAssertEqual(move(.pageDown), CellPosition(row: 24, column: 2))
        XCTAssertEqual(move(.pageUp), CellPosition(row: 0, column: 2))
        XCTAssertEqual(move(.firstRow), CellPosition(row: 0, column: 2))
        XCTAssertEqual(move(.lastRow), CellPosition(row: 99, column: 2))
        XCTAssertEqual(move(.firstColumn), CellPosition(row: 5, column: 0))
        XCTAssertEqual(move(.lastColumn), CellPosition(row: 5, column: 3))
        XCTAssertEqual(move(.next, from: CellPosition(row: 5, column: 3)), CellPosition(row: 6, column: 0))
        XCTAssertEqual(move(.previous, from: CellPosition(row: 5, column: 0)), CellPosition(row: 4, column: 3))
        // At the edges, moves stop.
        XCTAssertEqual(move(.up, from: CellPosition(row: 0, column: 0)), CellPosition(row: 0, column: 0))
        XCTAssertEqual(move(.next, from: CellPosition(row: 99, column: 3)), CellPosition(row: 99, column: 3))
        XCTAssertEqual(move(.previous, from: CellPosition(row: 0, column: 0)), CellPosition(row: 0, column: 0))
        XCTAssertEqual(move(.pageDown, from: CellPosition(row: 95, column: 0)), CellPosition(row: 99, column: 0))
        XCTAssertEqual(GridMove.down.apply(to: start, rows: 0, columns: 4, pageRows: 20), start)
    }

    func testStandardKeyBindingsBecomeMoves() {
        XCTAssertEqual(GridMove(selector: #selector(NSResponder.moveDown(_:))), .down)
        XCTAssertEqual(GridMove(selector: #selector(NSResponder.moveToEndOfDocument(_:))), .lastRow)
        XCTAssertEqual(GridMove(selector: #selector(NSResponder.scrollPageDown(_:))), .pageDown)
        XCTAssertEqual(GridMove(selector: #selector(NSResponder.insertTab(_:))), .next)
        XCTAssertNil(GridMove(selector: #selector(NSResponder.insertNewline(_:))))
    }

    // MARK: The tile cache

    func testTilesAreReadOnceAndAgainWhenMoreRowsArrive() {
        var reads: [(Range<Int>, Range<Int>)] = []
        var available = 10
        let cache = CellTileCache(capacity: 4) { rows, columns in
            reads.append((rows, columns))
            return rows.clamped(to: 0..<available).map { row in
                // Row r has r % 5 + 1 fields.
                let fields = row % 5 + 1
                return TileRow(
                    fieldCount: fields,
                    cells: columns.clamped(to: 0..<fields).map { .text("\(row):\($0)", truncated: false) }
                )
            }
        }
        XCTAssertEqual(cache.cell(row: 3, column: 2, loadedRows: available), .text("3:2", truncated: false))
        XCTAssertEqual(cache.cell(row: 3, column: 4, loadedRows: available), .missing)
        XCTAssertEqual(cache.cell(row: 4, column: 4, loadedRows: available), .text("4:4", truncated: false))
        XCTAssertEqual(cache.cell(row: 12, column: 0, loadedRows: available), .notLoaded)
        XCTAssertEqual(reads.count, 1)
        XCTAssertEqual(cache.widestRow, 5)
        // More rows indexed: the short tile is read again.
        available = 30
        XCTAssertEqual(cache.cell(row: 12, column: 0, loadedRows: available), .text("12:0", truncated: false))
        XCTAssertEqual(reads.count, 2)
        XCTAssertEqual(cache.cell(row: 20, column: 0, loadedRows: available), .text("20:0", truncated: false))
        XCTAssertEqual(reads.count, 2)
        // Another column block.
        XCTAssertEqual(cache.cell(row: 0, column: 40, loadedRows: available), .missing)
        XCTAssertEqual(reads.count, 3)
        XCTAssertEqual(reads.last?.1, 32..<64)
        // Prepare reads a region in one go; then cells come from the cache.
        available = 200
        cache.prepare(rows: 60..<130, columns: 0..<10, loadedRows: 200)
        let before = reads.count
        _ = cache.cell(row: 100, column: 1, loadedRows: 200)
        XCTAssertEqual(reads.count, before)
        XCTAssertEqual(Set(reads.suffix(2).map(\.0.lowerBound)), [64, 128])
    }

    func testTheLeastRecentlyUsedTilesAreDropped() {
        var reads = 0
        let cache = CellTileCache(capacity: 2) { rows, _ in
            reads += 1
            return rows.map { _ in TileRow(fieldCount: 1, cells: [.text("x", truncated: false)]) }
        }
        let rows = 1_000
        for block in 0..<3 {
            _ = cache.cell(row: block * CellTileCache.rowsPerTile, column: 0, loadedRows: rows)
        }
        XCTAssertEqual(reads, 3)
        // The first block was dropped and is read again.
        _ = cache.cell(row: 0, column: 0, loadedRows: rows)
        XCTAssertEqual(reads, 4)
    }

    func testAFailedReadIsNotRetried() {
        var reads = 0
        let cache = CellTileCache { _, _ in
            reads += 1
            return nil
        }
        XCTAssertEqual(cache.cell(row: 0, column: 0, loadedRows: 10), .notLoaded)
        XCTAssertEqual(cache.cell(row: 1, column: 0, loadedRows: 10), .notLoaded)
        XCTAssertEqual(reads, 1)
        cache.removeAll()
        _ = cache.cell(row: 0, column: 0, loadedRows: 10)
        XCTAssertEqual(reads, 2)
    }
}
