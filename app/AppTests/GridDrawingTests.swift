import AppKit
import CoreText
import XCTest

@testable import Leal

/// Task 2.0a's cheaper drawing draws exactly what the grid drew before:
/// text batched across cells (`GlyphBatch`), fills and separators in one
/// call each, lines laid out off the main thread (`LineMaker`), find's mark
/// offsets from one pass. Each is compared with the calls it replaces,
/// pixel for pixel or value for value. Then the reads and layout ahead of
/// the scroll: off the main thread, bounded, and dropped when what they
/// were for changed.
@MainActor
final class GridDrawingTests: XCTestCase {
    /// Values like the reference file's, and the awkward ones: fallback
    /// fonts, right-to-left and mixed-direction text, combining marks,
    /// emoji, control characters (drawn as symbols in another colour), and
    /// long values cut to the column.
    private let values = [
        "Toronto", "SKU-60835", "479.17", "2022-12-14", "Martin, Ana", "O'Brien, Mateo", "AV To Ty Wa", "",
        "서울", "東京", "שלום עולם", "مرحبا", "SKU-שלום 12", "e\u{301}te\u{301}", "👍🏽 ok", "👍🏽🎉", "🎉",
        "a\tb", "line\r\nbreak", "nul\u{0}x",
        "on address wrap arrival quickly before the end of the column", "1,234,567.89", "-0.15", "fi fl ffi",
    ]

    private let light = NSAppearance(named: .aqua)!
    private let dark = NSAppearance(named: .darkAqua)!

    /// A bitmap at `scale`, flipped like a view's, after `body` drew in it
    /// with `appearance` as the drawing appearance (white first).
    private func render(
        width: CGFloat = 300,
        height: CGFloat,
        scale: CGFloat = 2,
        appearance: NSAppearance? = nil,
        _ body: (CGContext) -> Void
    ) -> Data {
        let pixelsWide = Int(width * scale)
        let pixelsHigh = Int(height * scale)
        let context = CGContext(
            data: nil,
            width: pixelsWide,
            height: pixelsHigh,
            bitsPerComponent: 8,
            bytesPerRow: pixelsWide * 4,
            space: CGColorSpace(name: CGColorSpace.sRGB)!,
            bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue
        )!
        context.translateBy(x: 0, y: CGFloat(pixelsHigh))
        context.scaleBy(x: scale, y: -scale)
        context.setFillColor(CGColor(gray: 1, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let graphics = NSGraphicsContext(cgContext: context, flipped: true)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = graphics
        (appearance ?? light).performAsCurrentDrawingAppearance {
            body(context)
        }
        NSGraphicsContext.restoreGraphicsState()
        return Data(bytes: context.data!, count: pixelsWide * pixelsHigh * 4)
    }

    private func performDrawing<T>(_ appearance: NSAppearance? = nil, _ body: () -> T) -> T {
        var result: T?
        (appearance ?? light).performAsCurrentDrawingAppearance { result = body() }
        return result!
    }

    /// The cells of `values`, one a row, in a column `width` wide; numbers
    /// right-aligned in their font.
    private func cells(width: CGFloat) -> [(value: String, rect: CGRect, number: Bool)] {
        values.enumerated().map { index, value in
            let number = value.first?.isNumber == true || value.first == "-"
            return (value, CGRect(x: 10, y: CGFloat(index) * 22, width: width, height: 22), number)
        }
    }

    // MARK: Drawing

    func testBatchedTextDrawsTheSamePixelsAsCoreText() {
        for (appearance, scale) in [(light, 2.0), (dark, 2.0), (light, 1.0)] {
            for width: CGFloat in [260, 90, 40] {
                for truncated in [false, true] {
                    let cells = cells(width: width)
                    let height = CGFloat(cells.count) * 22
                    let palette = performDrawing(appearance) { GridPalette.current() }
                    let lines = cells.map { cell in
                        let request = LineRequest(value: cell.value, truncated: truncated, width: width, number: cell.number)
                        let font = cell.number ? GridFonts.number : GridFonts.cell
                        return performDrawing(appearance) { CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette) }
                    }
                    let reference = render(height: height, scale: scale, appearance: appearance) { context in
                        for (cell, line) in zip(cells, lines) {
                            let font = cell.number ? GridFonts.number : GridFonts.cell
                            CellPainter.drawText(line, in: cell.rect, font: font, alignment: cell.number ? .trailing : .leading, context: context, ellipsisColor: palette.text)
                        }
                    }
                    let batch = GlyphBatch()
                    let batched = render(height: height, scale: scale, appearance: appearance) { context in
                        for (cell, line) in zip(cells, lines) {
                            let font = cell.number ? GridFonts.number : GridFonts.cell
                            CellPainter.addText(line, in: cell.rect, font: font, alignment: cell.number ? .trailing : .leading, to: batch, context: context, ellipsisColor: palette.text)
                        }
                        batch.draw(in: context)
                    }
                    XCTAssertEqual(reference, batched, "\(appearance.name.rawValue) at \(scale)×, width \(width), truncated \(truncated)")
                }
            }
        }
    }

    func testALineOfSeveralColoursIsNotBatched() {
        let palette = performDrawing { GridPalette.current() }
        let symbols = performDrawing { CellPainter.makeCellLine("a\tb", truncated: false, font: GridFonts.cell, palette: palette) }
        let plain = performDrawing { CellPainter.makeCellLine("ab", truncated: false, font: GridFonts.cell, palette: palette) }
        let batch = GlyphBatch()
        XCTAssertFalse(batch.add(symbols.runs ?? [], x: 0, baseline: 10))
        XCTAssertTrue(batch.add(plain.runs ?? [], x: 0, baseline: 10))
    }

    func testFillsInOneCallDrawTheSamePixels() {
        let palette = performDrawing { GridPalette.current() }
        // Two lines in one place (a column of no width) and one half a
        // point on: each as dark as two separate fills make it.
        let xs: [CGFloat] = [40, 120, 120, 120.5, 121, 260.5]
        let reference = render(height: 220) { context in
            for row in 3..<12 {
                context.setFillColor(palette.rowBackgrounds[row % palette.rowBackgrounds.count])
                context.fill(CGRect(x: 7, y: CGFloat(row) * 22 - 60, width: 280, height: 22))
            }
            for x in xs {
                CellPainter.drawColumnSeparator(atX: x, minY: 5, maxY: 190, palette: palette, context: context)
            }
        }
        var scratch: [CGRect] = []
        let batched = render(height: 220) { context in
            context.translateBy(x: 0, y: -60)
            CellPainter.drawRowBackgrounds(rows: 3..<12, minX: 7, width: 280, rowHeight: 22, palette: palette, context: context, scratch: &scratch)
            context.translateBy(x: 0, y: 60)
            CellPainter.drawColumnSeparators(atX: xs, minY: 5, maxY: 190, palette: palette, context: context, scratch: &scratch)
        }
        XCTAssertEqual(reference, batched)
    }

    /// The whole grid, drawn by `GridView`, against the same cells drawn the
    /// way the grid drew them before 2.0a: a call per row's shading, per
    /// cell's fill, mark and text, and per separator. With a selection, a
    /// find mark and the current match, hatched missing cells, skeleton
    /// rows, a column of no width, and a value whose stacked marks spill
    /// into the selected cell below it; light and dark, at 2× and 1×.
    func testTheGridDrawsWhatItDrewBeforeBatching() {
        let source = DrawingSource()
        let highlighter = DrawingHighlighter()
        let widths: [CGFloat] = [110, 90, 0, 120, 80]
        let rows = source.rowCount
        let size = CGSize(width: widths.reduce(0, +) + 20, height: CGFloat(rows + 2) * 22)
        let selection = GridSelection(active: CellPosition(row: 4, column: 0), anchor: CellPosition(row: 4, column: 0), extent: CellPosition(row: 5, column: 1))
        for (appearance, scale) in [(light, 2.0), (dark, 2.0), (light, 1.0)] {
            let grid = GridView(frame: CGRect(origin: .zero, size: size))
            grid.appearance = appearance
            grid.geometry = GridLayout(widths: widths)
            grid.dataSource = source
            grid.highlighter = highlighter
            grid.selection = selection
            let drawn = render(width: size.width, height: size.height, scale: scale, appearance: appearance) { _ in
                grid.draw(CGRect(origin: .zero, size: size))
            }
            let reference = render(width: size.width, height: size.height, scale: scale, appearance: appearance) { context in
                drawAsBefore(source: source, highlighter: highlighter, geometry: grid.geometry, selection: selection, size: size, context: context)
            }
            XCTAssertEqual(drawn, reference, "\(appearance.name.rawValue) at \(scale)×")
        }
    }

    /// The gutter, drawn by `GridGutterView`, against its drawing before
    /// 2.0a: a call per row's number, then its marker. In a narrow gutter
    /// a seven-digit number reaches under the marker's dot, which must
    /// still be drawn over it; with the active row and selected rows, light
    /// and dark, 2× and 1×.
    func testTheGutterDrawsWhatItDrewBeforeBatching() {
        let source = MarkedSource()
        let first = 999_990
        let rows = first..<1_000_006
        let width: CGFloat = 44
        let dirty = CGRect(x: 0, y: CGFloat(rows.lowerBound) * 22, width: width, height: CGFloat(rows.count) * 22)
        for (appearance, scale) in [(light, 2.0), (dark, 2.0), (light, 1.0)] {
            let gutter = GridGutterView(frame: CGRect(x: 0, y: 0, width: width, height: CGFloat(source.rowCount) * 22))
            gutter.appearance = appearance
            gutter.dataSource = source
            gutter.activeRow = first + 3
            gutter.selectedRows = (first + 6)...(first + 8)
            let drawn = render(width: width, height: dirty.height, scale: scale, appearance: appearance) { context in
                context.translateBy(x: 0, y: -dirty.minY)
                gutter.draw(dirty)
            }
            let reference = render(width: width, height: dirty.height, scale: scale, appearance: appearance) { context in
                context.translateBy(x: 0, y: -dirty.minY)
                let palette = GridPalette.current()
                context.setFillColor(palette.gutterBackground)
                context.fill(dirty)
                context.setFillColor(palette.gridLine)
                context.fill(CGRect(x: width - 1, y: dirty.minY, width: 1, height: dirty.height))
                for row in rows {
                    let rect = CGRect(x: 0, y: CGFloat(row) * 22, width: width - 4, height: 22)
                    let active = row == first + 3
                    let accent = active || (first + 6...first + 8).contains(row)
                    let font = active ? GridFonts.gutterActive : GridFonts.gutter
                    let color = accent ? palette.accent : palette.secondaryText
                    let line = CellPainter.makeLine(String(row + 1), font: font, color: color, symbolColor: color)
                    CellPainter.drawText(line, in: rect, font: font, alignment: .trailing, context: context, ellipsisColor: palette.secondaryText)
                    if source.rowHasMarker(row) {
                        CellPainter.drawGutterMarker(rowRect: rect, context: context)
                    }
                }
            }
            XCTAssertEqual(drawn, reference, "\(appearance.name.rawValue) at \(scale)×")
        }
    }

    /// `GridView.draw` as it was before 2.0a (main at 5247459).
    private func drawAsBefore(
        source: DrawingSource,
        highlighter: DrawingHighlighter,
        geometry: GridLayout,
        selection: GridSelection,
        size: CGSize,
        context: CGContext
    ) {
        let palette = GridPalette.current()
        let dirty = CGRect(origin: .zero, size: size)
        for row in geometry.rowRange(minY: dirty.minY, maxY: dirty.maxY, rows: Int((size.height / geometry.rowHeight).rounded(.up))) {
            context.setFillColor(palette.rowBackgrounds[row % palette.rowBackgrounds.count])
            context.fill(CGRect(x: dirty.minX, y: CGFloat(row) * geometry.rowHeight, width: dirty.width, height: geometry.rowHeight))
        }
        let rows = geometry.rowRange(minY: dirty.minY, maxY: dirty.maxY, rows: source.rowCount)
        let columns = geometry.columnRange(minX: dirty.minX, maxX: dirty.maxX)
        for row in rows {
            for column in columns {
                let rect = geometry.cellRect(row: row, column: column)
                let number = source.isNumeric(column: column)
                let alignment: CellAlignment = number ? .trailing : .leading
                let highlight = highlighter.highlight(row: row, column: column)
                if selection.contains(row: row, column: column), !(highlight?.isCurrent == true && highlight?.ranges.isEmpty == false) {
                    CellPainter.drawActiveCellFill(in: rect, palette: palette, context: context)
                }
                guard row < source.loadedRowCount else {
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                    continue
                }
                switch source.cell(row: row, column: column) {
                case let .text(value, truncated):
                    guard !value.isEmpty || truncated else { continue }
                    let font = number ? GridFonts.number : GridFonts.cell
                    let request = LineRequest(value: value, truncated: truncated, width: rect.width, number: number)
                    let line = CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette)
                    if let highlight, !highlight.ranges.isEmpty {
                        CellPainter.drawFindHighlights(
                            line,
                            ranges: CellText.displayRanges(highlight.ranges, in: request.shown),
                            in: rect,
                            alignment: alignment,
                            current: highlight.isCurrent,
                            palette: palette,
                            context: context
                        )
                    }
                    CellPainter.drawText(line, in: rect, font: font, alignment: alignment, context: context, ellipsisColor: palette.text)
                case .notLoaded:
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                case .missing:
                    if source.isHatched(row: row, column: column) {
                        CellPainter.drawHatch(in: rect, palette: palette, context: context)
                    }
                }
            }
        }
        for column in columns {
            CellPainter.drawColumnSeparator(atX: geometry.offsets[column + 1], minY: dirty.minY, maxY: dirty.maxY, palette: palette, context: context)
        }
        let active = selection.active
        let current = highlighter.highlight(row: active.row, column: active.column)
        if !(current?.isCurrent == true && current?.ranges.isEmpty == false) {
            CellPainter.drawActiveCellRing(in: geometry.cellRect(row: active.row, column: active.column), palette: palette, context: context)
        }
    }

    func testCaretOffsetsAgreeWithCoreText() {
        let palette = performDrawing { GridPalette.current() }
        for value in values + ["SKU-SKU-SKU-", "  spaced  ", "x"] {
            for font in [GridFonts.cell, GridFonts.number] {
                let line = performDrawing { CellPainter.makeCellLine(value, truncated: false, font: font, palette: palette) }
                let length = CTLineGetStringRange(line.line).length
                for index in 0...length {
                    XCTAssertEqual(line.offset(at: index), CTLineGetOffsetForStringIndex(line.line, index, nil), "\(value) at \(index)")
                }
            }
        }
    }

    /// `LineMaker`, off the main thread, makes the line the draw would.
    func testLinesMadeAheadAreTheLinesTheDrawMakes() async {
        let palette = performDrawing { GridPalette.current() }
        let maker = LineMaker(palette: palette, caretOffsets: true)
        for width: CGFloat in [260, 70, 40] {
            for (index, value) in values.enumerated() {
                let number = index % 3 == 0
                let truncated = index % 4 == 0
                let request = LineRequest(value: value, truncated: truncated, width: width, number: number)
                let available = width - 2 * GridMetrics.cellPadding
                let item = LineMaker.Item(request: request, available: available)
                let ahead = TextLine(await Task.detached { maker.make(item) }.value)
                let font = number ? GridFonts.number : GridFonts.cell
                let here = performDrawing { CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette) }
                XCTAssertEqual(ahead.width, here.width, value)
                XCTAssertEqual(glyphs(ahead.line), glyphs(here.line), value)
                for index in 0...CTLineGetStringRange(here.line).length {
                    XCTAssertEqual(ahead.offset(at: index), CTLineGetOffsetForStringIndex(here.line, index, nil), "\(value) at \(index)")
                }
                guard available > 2, here.width > available else {
                    XCTAssertNil(ahead.fitted, value)
                    continue
                }
                // As `CellPainter.drawText` cuts it.
                performDrawing {
                    let ellipsis = CellPainter.makeLine("…", font: font, color: palette.text, symbolColor: palette.text)
                    here.fit(available, ellipsis: ellipsis.line)
                }
                XCTAssertEqual(ahead.fitted?.available, here.fitted?.available, value)
                XCTAssertEqual(ahead.fitted?.width, here.fitted?.width, value)
                XCTAssertEqual(ahead.fitted?.line.map(glyphs), here.fitted?.line.map(glyphs), value)
            }
        }
    }

    /// A line's glyphs, positions and fonts.
    private func glyphs(_ line: CTLine) -> [String] {
        (CTLineGetGlyphRuns(line) as? [CTRun] ?? []).map { run in
            let count = CTRunGetGlyphCount(run)
            var glyphs = [CGGlyph](repeating: 0, count: count)
            var positions = [CGPoint](repeating: .zero, count: count)
            CTRunGetGlyphs(run, CFRange(location: 0, length: count), &glyphs)
            CTRunGetPositions(run, CFRange(location: 0, length: count), &positions)
            let attributes = CTRunGetAttributes(run) as NSDictionary
            let font = (attributes[kCTFontAttributeName] as AnyObject?).map { CTFontCopyPostScriptName($0 as! CTFont) as String } ?? "?"
            return "\(font) \(glyphs) \(positions)"
        }
    }

    func testThePaletteIsMadeOnceForEachAppearance() {
        // (The header's colour is made with the palette, not taken from
        // the system's, so it is a new object each time one is made.)
        let palette = performDrawing { GridPalette.current() }
        XCTAssertTrue(palette.headerBackground === performDrawing { GridPalette.current() }.headerBackground)
        let darkPalette = performDrawing(dark) { GridPalette.current() }
        XCTAssertFalse(palette.rowBackgrounds[0] == darkPalette.rowBackgrounds[0])
        // A change to the system's colours (the accent colour) makes it again,
        let before = performDrawing { GridPalette.current() }
        NotificationCenter.default.post(name: NSColor.systemColorsDidChangeNotification, object: nil)
        let after = performDrawing { GridPalette.current() }
        XCTAssertFalse(before.headerBackground === after.headerBackground)
        // and so does Increase Contrast, an accessibility display option.
        NSWorkspace.shared.notificationCenter.post(name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil)
        XCTAssertFalse(after.headerBackground === performDrawing { GridPalette.current() }.headerBackground)
    }

    // MARK: Reading ahead

    /// Spins the main run loop until `condition` holds: work that comes back
    /// to the main actor, signalled by a count it keeps.
    private func waitUntil(_ what: String, timeout: TimeInterval = 10, _ condition: () -> Bool) {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            guard Date() < deadline else {
                XCTFail("timed out waiting until \(what)")
                return
            }
            RunLoop.main.run(until: Date().addingTimeInterval(0.005))
        }
    }

    nonisolated private static func tile(_ rows: Range<Int>, _ columns: Range<Int>, _ text: String = "") -> [TileRow] {
        rows.map { row in TileRow(fieldCount: 3, cells: columns.clamped(to: 0..<3).map { .text("\(text)\(row):\($0)", truncated: false) }) }
    }

    func testTilesAreReadAheadOffTheMainThread() {
        let reads = Counter()
        let cache = CellTileCache { rows, _ in
            XCTFail("the draw read \(rows) itself")
            return nil
        }
        cache.makeBackgroundFetch = {
            { rows, columns in
                XCTAssertFalse(Thread.isMainThread)
                reads.add()
                return Self.tile(rows, columns)
            }
        }
        XCTAssertNil(cache.cachedCell(row: 70, column: 1, loadedRows: 1_000))
        cache.readAhead(rows: 60..<140, columns: 0..<3, loadedRows: 1_000)
        // Asking again while they are being read reads nothing more.
        cache.readAhead(rows: 60..<140, columns: 0..<3, loadedRows: 1_000)
        waitUntil("the three are back") { cache.readsAheadBack == 3 }
        XCTAssertEqual(reads.value, 3, "row blocks 0, 1 and 2, once each")
        XCTAssertEqual(cache.cachedCell(row: 70, column: 1, loadedRows: 1_000), .text("70:1", truncated: false))
        XCTAssertEqual(cache.cachedCell(row: 70, column: 5, loadedRows: 1_000), .missing)
        XCTAssertEqual(cache.cell(row: 139, column: 2, loadedRows: 1_000), .text("139:2", truncated: false))
        XCTAssertEqual(cache.fetchCount, 0)
        // Rows not indexed yet aren't read.
        cache.readAhead(rows: 900..<1_100, columns: 0..<3, loadedRows: 1_000)
        waitUntil("the two are back") { cache.readsAheadBack == 5 }
        XCTAssertEqual(reads.value, 5, "row blocks 14 and 15, not the ones past row 1,000")
    }

    /// Item 1 of the review: a jump across a long file asks for a few tiles
    /// at a time, however long the region.
    func testAHugeRegionStartsOnlyAFewReads() {
        let cache = CellTileCache { _, _ in nil }
        cache.makeBackgroundFetch = { { _, _ in [] } }
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 0..<1_600_000, columns: 0..<40, loadedRows: 2_000_000)
        XCTAssertEqual(cache.readsAheadStarted, CellTileCache.newReadsPerCall)
        for _ in 0..<10 {
            cache.readAhead(rows: 0..<1_600_000, columns: 0..<40, loadedRows: 2_000_000)
        }
        XCTAssertEqual(cache.readsAheadUnderWay, CellTileCache.readsInFlight)
        XCTAssertEqual(cache.readsAheadStarted, CellTileCache.readsInFlight)
        CellTileCache.resumeReadsAhead()
        waitUntil("they are back") { cache.readsAheadBack == CellTileCache.readsInFlight }
    }

    /// A read queued for rows the scroll has since left isn't made.
    func testAReadTheScrollHasLeftIsSkipped() {
        let reads = Counter()
        let cache = CellTileCache { _, _ in nil }
        cache.makeBackgroundFetch = {
            { rows, _ in
                reads.add()
                return rows.map { _ in TileRow(fieldCount: 1, cells: [.text("x", truncated: false)]) }
            }
        }
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 0..<64, columns: 0..<1, loadedRows: 1_000_000)
        // A draw far away starts what is wanted again (and reads its two
        // neighbours ahead).
        cache.prepare(rows: 512_000..<512_064, columns: 0..<1, loadedRows: 1_000_000)
        CellTileCache.resumeReadsAhead()
        waitUntil("all three are back") { cache.readsAheadBack == 3 }
        XCTAssertEqual(reads.value, 2, "only the blocks still wanted")
        XCTAssertNil(cache.cachedCell(row: 0, column: 0, loadedRows: 1_000_000))
        XCTAssertNotNil(cache.cachedCell(row: 512_064, column: 0, loadedRows: 1_000_000))
    }

    /// Between draws, what is asked for ahead only widens what is wanted.
    func testReadsAheadBetweenDrawsAreAllWanted() {
        let reads = Counter()
        let cache = CellTileCache { _, _ in nil }
        cache.makeBackgroundFetch = {
            { rows, _ in
                reads.add()
                return rows.map { _ in TileRow(fieldCount: 1, cells: [.text("x", truncated: false)]) }
            }
        }
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 0..<64, columns: 0..<1, loadedRows: 1_000_000)
        cache.readAhead(rows: 512_000..<512_064, columns: 0..<1, loadedRows: 1_000_000)
        CellTileCache.resumeReadsAhead()
        waitUntil("both are back") { cache.readsAheadBack == 2 }
        XCTAssertEqual(reads.value, 2)
    }

    /// Review nit 7: the cache keeps room for a wide region's tiles, so a
    /// window of many column blocks doesn't evict what it just read.
    func testTheCacheGrowsWithTheRegion() {
        var reads = 0
        let cache = CellTileCache(capacity: 2) { rows, columns in
            reads += 1
            return rows.map { _ in TileRow(fieldCount: 400, cells: columns.map { .text("c\($0)", truncated: false) }) }
        }
        cache.makeBackgroundFetch = { { _, _ in nil } }
        // The first read finds rows 400 fields wide: 13 column blocks.
        _ = cache.cell(row: 0, column: 0, loadedRows: 100)
        cache.prepare(rows: 0..<30, columns: 0..<400, loadedRows: 100)
        let read = reads
        for column in stride(from: 0, to: 400, by: 32) {
            _ = cache.cell(row: 0, column: column, loadedRows: 100)
        }
        XCTAssertEqual(reads, read, "a tile of the region was dropped and read again")
        waitUntil("the reads ahead are back") { cache.readsAheadBack == cache.readsAheadStarted }
    }

    func testTilesReadAheadBeforeTheCacheIsEmptiedAreDropped() {
        let cache = CellTileCache { rows, columns in Self.tile(rows, columns, "new ") }
        cache.makeBackgroundFetch = { { rows, columns in Self.tile(rows, columns, "old ") } }
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        cache.removeAll()
        CellTileCache.resumeReadsAhead()
        waitUntil("the read is back") { cache.readsAheadBack == 1 }
        XCTAssertEqual(cache.readAheadCount, 0)
        XCTAssertEqual(cache.cell(row: 0, column: 0, loadedRows: 10), .text("new 0:0", truncated: false))
    }

    /// An old read coming back late doesn't take the place of a newer one,
    /// asked for after the cache was emptied, nor forget that it is under
    /// way.
    func testAStaleReadLeavesANewerOneAlone() {
        let answers = Counter()
        let cache = CellTileCache { _, _ in nil }
        cache.makeBackgroundFetch = {
            { rows, columns in
                let n = answers.next()
                return Self.tile(rows, columns, "read \(n) ")
            }
        }
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        cache.removeAll()
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        CellTileCache.resumeReadsAhead()
        waitUntil("the first is back") { cache.readsAheadBack >= 1 }
        waitUntil("both are back") { cache.readsAheadBack == 2 }
        XCTAssertEqual(cache.readsAheadUnderWay, 0)
        XCTAssertEqual(cache.cachedCell(row: 0, column: 0, loadedRows: 10), .text("read 2 0:0", truncated: false))
    }

    /// Item 3: an edit's rows, read again; a read of them under way when
    /// they changed is thrown away when it comes back.
    func testChangedRowsAreReadAgainAndAReadUnderWayIsDropped() {
        var version = 1
        let cache = CellTileCache { rows, columns in Self.tile(rows, columns, "v\(version) ") }
        cache.makeBackgroundFetch = { { rows, columns in Self.tile(rows, columns, "ahead ") } }
        XCTAssertEqual(cache.cell(row: 5, column: 0, loadedRows: 1_000), .text("v1 5:0", truncated: false))
        CellTileCache.suspendReadsAhead()
        defer { CellTileCache.resumeReadsAhead() }
        cache.readAhead(rows: 64..<128, columns: 0..<1, loadedRows: 1_000)
        version = 2
        cache.invalidate(rows: 5..<70)
        CellTileCache.resumeReadsAhead()
        waitUntil("the read is back") { cache.readsAheadBack == 1 }
        XCTAssertNil(cache.cachedCell(row: 70, column: 0, loadedRows: 1_000), "the read under way was dropped")
        XCTAssertEqual(cache.cell(row: 5, column: 0, loadedRows: 1_000), .text("v2 5:0", truncated: false))
        XCTAssertEqual(cache.cell(row: 70, column: 0, loadedRows: 1_000), .text("v2 70:0", truncated: false))
    }

    /// Item 5: an error comes back to the document, and the tile isn't
    /// asked for ahead again; the draw still reads it.
    func testAFailedReadAheadIsReportedAndNotAskedForAgain() {
        struct Gone: Error {}
        var reported: [any Error] = []
        let cache = CellTileCache { rows, columns in Self.tile(rows, columns) }
        cache.makeBackgroundFetch = { { _, _ in throw Gone() } }
        cache.onReadAheadError = { reported.append($0) }
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        waitUntil("the read is back") { cache.readsAheadBack == 1 }
        XCTAssertEqual(reported.count, 1)
        XCTAssertTrue(reported.first is Gone)
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        XCTAssertEqual(cache.readsAheadStarted, 1)
        XCTAssertNil(cache.cachedCell(row: 0, column: 0, loadedRows: 10))
        XCTAssertEqual(cache.cell(row: 0, column: 0, loadedRows: 10), .text("0:0", truncated: false))
        // Emptied (a Reload, another reading), it may be asked for again.
        cache.removeAll()
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        XCTAssertEqual(cache.readsAheadStarted, 2)
        waitUntil("the second is back") { cache.readsAheadBack == 2 }
    }

    /// A grid in a window that is never shown, with `source`.
    private func makeGrid(_ source: any GridDataSource, columns: Int) -> GridContainerView {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 466), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        addTeardownBlock { window.close() }
        let grid = GridContainerView(frame: window.contentLayoutRect)
        window.contentView = grid
        grid.dataSource = source
        grid.setColumnWidths(Array(repeating: 100, count: columns))
        grid.layoutSubtreeIfNeeded()
        return grid
    }

    private func draw(_ view: NSView) throws {
        view.cacheDisplay(in: view.visibleRect, to: try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.visibleRect)))
    }

    private func scroll(_ grid: GridContainerView, toRow row: Int) {
        let clip = grid.scrollView.contentView
        clip.scroll(to: NSPoint(x: 0, y: CGFloat(row) * 22))
        grid.scrollView.reflectScrolledClipView(clip)
    }

    func testTheGridLaysOutTheRowsAheadOfTheScroll() throws {
        let source = FakeGridSource(rows: 10_000, columns: 3)
        source.text = { row, column in "r\(row)c\(column)" }
        let grid = makeGrid(source, columns: 3)
        let gridView = grid.gridView
        try draw(gridView)
        scroll(grid, toRow: 10)
        try draw(gridView)
        // Rows below the visible ones are laid out: their lines are cached
        // before any draw asks for them.
        let below = Int(grid.scrollView.contentView.bounds.maxY / 22) + 2
        let key = LineRequest(value: "r\(below)c0", truncated: false, width: 100, number: false).key
        waitUntil("the lines are made") { gridView.lines.contains(key) }
        XCTAssertGreaterThan(gridView.ahead.linesMade, 0)
    }

    /// Items 1 and 6f: a jump of 400,000 rows (a scroller drag, a click in
    /// its track, Go to Row, Find's next match far away) asks for at most a
    /// screen of lines and numbers, and reads ahead at most two screens of
    /// rows, not the 1.6M rows four jumps' travel would be.
    func testAJumpLaysOutAndReadsOnlyAScreenOrTwo() throws {
        let source = ReadAheadSource(rows: 1_000_000, columns: 3)
        let grid = makeGrid(source, columns: 3)
        let gridView = grid.gridView
        let screenRows = Int((grid.scrollView.contentView.bounds.height / 22).rounded(.up))
        try draw(gridView)
        try draw(grid.gutterView)
        waitUntil("the first lines are made") { gridView.ahead.batchesBack == gridView.ahead.batchesSent }
        let made = gridView.ahead.linesMade
        let numbers = grid.gutterView.numbersAskedAhead
        source.readsAhead.removeAll()
        source.uncached = 400_000..<1_000_000
        scroll(grid, toRow: 400_000)
        try draw(gridView)
        try draw(grid.gutterView)
        waitUntil("the jump's lines are made") { gridView.ahead.batchesBack == gridView.ahead.batchesSent }
        XCTAssertLessThanOrEqual(gridView.ahead.linesMade - made, screenRows * 3, "at most a screen of rows")
        XCTAssertLessThanOrEqual(grid.gutterView.numbersAskedAhead - numbers, screenRows)
        XCTAssertFalse(source.readsAhead.isEmpty)
        for rows in source.readsAhead {
            XCTAssertLessThanOrEqual(rows.count, 2 * screenRows, "\(rows)")
            XCTAssertTrue(rows.lowerBound >= 400_000 - screenRows && rows.upperBound <= 400_000 + 2 * screenRows, "\(rows)")
        }
    }

    /// Item 6a: lines made for the old colours or values, still being made
    /// when they changed (dark mode, Increase Contrast, a Reload, column
    /// styles: each calls `LineReadAhead.reset`, through
    /// `viewDidChangeEffectiveAppearance` or `invalidateContent`), aren't
    /// kept. Called here directly, so no draw follows to make them again.
    func testLinesMadeBeforeAResetAreDropped() throws {
        let source = FakeGridSource(rows: 10_000, columns: 3)
        source.text = { row, column in "r\(row)c\(column)" }
        let grid = makeGrid(source, columns: 3)
        let gridView = grid.gridView
        try draw(gridView)
        waitUntil("the first lines are made") { gridView.ahead.batchesBack == gridView.ahead.batchesSent }
        LineReadAhead.suspendForTesting()
        defer { LineReadAhead.resumeForTesting() }
        scroll(grid, toRow: 10)
        try draw(gridView)
        let sent = gridView.ahead.batchesSent
        let below = Int(grid.scrollView.contentView.bounds.maxY / 22) + 2
        let key = LineRequest(value: "r\(below)c0", truncated: false, width: 100, number: false).key
        XCTAssertFalse(gridView.lines.contains(key))
        gridView.ahead.reset()
        LineReadAhead.resumeForTesting()
        waitUntil("the batch is back") { gridView.ahead.batchesBack == sent }
        XCTAssertFalse(gridView.lines.contains(key), "a line made before the reset was kept")
        // Without the reset, it would have been.
        LineReadAhead.suspendForTesting()
        defer { LineReadAhead.resumeForTesting() }
        scroll(grid, toRow: 40)
        try draw(gridView)
        let later = Int(grid.scrollView.contentView.bounds.maxY / 22) + 2
        let laterKey = LineRequest(value: "r\(later)c0", truncated: false, width: 100, number: false).key
        LineReadAhead.resumeForTesting()
        waitUntil("the next batch is back") { gridView.ahead.batchesBack == gridView.ahead.batchesSent }
        XCTAssertTrue(gridView.lines.contains(laterKey))
    }
}

/// A data source for the whole-grid drawing test: names, numbers, a short
/// ragged row, a spilling value, rows not loaded.
@MainActor
private final class DrawingSource: GridDataSource {
    let rowCount = 12
    let loadedRowCount = 10
    let columnCount = 5

    func headerTitle(column: Int) -> HeaderTitle { HeaderTitle(text: "c\(column)", style: .name) }
    func isNumeric(column: Int) -> Bool { column == 1 }
    func prepare(rows: Range<Int>, columns: Range<Int>) {}

    func cell(row: Int, column: Int) -> GridCell {
        guard row < loadedRowCount else { return .notLoaded }
        if row == 6, column >= 2 { return .missing }
        // Stacked marks that spill out of their cell, onto the cell below:
        // the selected one (row 4), a skeleton (row 10), a hatch (row 6,
        // column 3) and a find mark (row 2, column 3). Each is drawn after
        // the spilling text, as before batching.
        let marks = "\u{0335}\u{0327}\u{0322}\u{031B}\u{031B}\u{031E}\u{032B}\u{0317}\u{0317}\u{0349}\u{032E}\u{0348}\u{031D}\u{032E}\u{0316}\u{031C}\u{032D}\u{0332}\u{0339}\u{0319}\u{033C}"
        switch (row, column) {
        // (Only the marks' cluster, so the line is one run in one font and
        // is batched.)
        case (3, 0), (9, 0), (1, 3), (5, 3): return .text("Z\(marks)", truncated: false)
        case (_, 1): return .text("\(row * 37).5", truncated: false)
        case (_, 3): return .text("SKU-\(60_000 + row) on a long note", truncated: row == 2)
        // A line of two colours, drawn on its own, only where it doesn't
        // stand between a spill and what it spills onto.
        case (7, 4): return .text("a\tb", truncated: false)
        case (_, 4): return .text("서울", truncated: false)
        default: return .text("Name \(row)", truncated: false)
        }
    }

    func isHatched(row: Int, column: Int) -> Bool { row == 6 }
}

/// A million rows, every third with a gutter marker, for the gutter test.
@MainActor
private final class MarkedSource: GridDataSource {
    let rowCount = 1_000_010
    let loadedRowCount = 1_000_010
    let columnCount = 1

    func headerTitle(column: Int) -> HeaderTitle { HeaderTitle(text: "c", style: .name) }
    func isNumeric(column: Int) -> Bool { false }
    func prepare(rows: Range<Int>, columns: Range<Int>) {}
    func cell(row: Int, column: Int) -> GridCell { .text("x", truncated: false) }
    func rowHasMarker(_ row: Int) -> Bool { row % 3 == 0 }
}

/// Find's marks for the drawing test: in column 3, rows 2 (the current
/// match) and 7.
@MainActor
private final class DrawingHighlighter: GridHighlighter {
    func prepareHighlights(rows: Range<Int>, columns: Range<Int>) {}

    func highlight(row: Int, column: Int) -> CellHighlight? {
        // Only rows 2 and 7: a mark flushes the text before it, and marks
        // in every row would hide whether the other flushes happen.
        guard column == 3, row == 2 || row == 7 else { return nil }
        return CellHighlight(ranges: [NSRange(location: 0, length: 1)], isCurrent: row == 2)
    }
}

/// A data source whose rows in `uncached` aren't read yet, recording what
/// the grid asks to read ahead.
@MainActor
private final class ReadAheadSource: GridDataSource {
    let rowCount: Int
    let loadedRowCount: Int
    let columnCount: Int
    var uncached: Range<Int> = 0..<0
    var readsAhead: [Range<Int>] = []

    init(rows: Int, columns: Int) {
        rowCount = rows
        loadedRowCount = rows
        columnCount = columns
    }

    func headerTitle(column: Int) -> HeaderTitle { HeaderTitle(text: "c\(column)", style: .name) }
    func isNumeric(column: Int) -> Bool { false }
    func prepare(rows: Range<Int>, columns: Range<Int>) {}
    func cell(row: Int, column: Int) -> GridCell { .text("r\(row)c\(column)", truncated: false) }

    func cachedCell(row: Int, column: Int) -> GridCell? {
        uncached.contains(row) ? nil : cell(row: row, column: column)
    }

    func readAhead(rows: Range<Int>, columns: Range<Int>) {
        readsAhead.append(rows)
        uncached = uncached.lowerBound..<uncached.lowerBound
    }
}

/// A count shared with background threads.
private final class Counter: @unchecked Sendable {
    // @unchecked: every access holds the lock.
    private let lock = NSLock()
    private var count = 0

    var value: Int { lock.withLock { count } }

    func add() {
        lock.withLock { count += 1 }
    }

    /// Adds one, and gives the new count.
    func next() -> Int {
        lock.withLock {
            count += 1
            return count
        }
    }
}
