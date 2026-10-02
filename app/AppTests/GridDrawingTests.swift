import AppKit
import CoreText
import XCTest

@testable import Leal

/// Task 2.0a's cheaper drawing draws exactly what the grid drew before:
/// text batched across cells (`GlyphBatch`), fills and separators in one
/// call each, lines laid out off the main thread (`LineMaker`), find's mark
/// offsets from the glyphs. Each is compared with the call it replaces,
/// pixel for pixel or value for value.
@MainActor
final class GridDrawingTests: XCTestCase {
    /// Values like the reference file's, and the awkward ones: fallback
    /// fonts, right-to-left text, combining marks, emoji, control
    /// characters (drawn as symbols in another colour), and long values
    /// cut to the column.
    private let values = [
        "Toronto", "SKU-60835", "479.17", "2022-12-14", "Martin, Ana", "O'Brien, Mateo", "AV To Ty Wa", "",
        "서울", "東京", "שלום עולם", "مرحبا", "e\u{301}te\u{301}", "👍🏽 ok", "a\tb", "line\r\nbreak", "nul\u{0}x",
        "on address wrap arrival quickly before the end of the column", "1,234,567.89", "-0.15", "fi fl ffi",
    ]

    private func appearance() -> NSAppearance { NSAppearance(named: .aqua)! }

    /// A white bitmap at 2×, flipped like a view's, after `body` drew in it.
    private func render(width: CGFloat = 300, height: CGFloat, _ body: (CGContext) -> Void) -> Data {
        let scale: CGFloat = 2
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
        appearance().performAsCurrentDrawingAppearance {
            body(context)
        }
        NSGraphicsContext.restoreGraphicsState()
        return Data(bytes: context.data!, count: pixelsWide * pixelsHigh * 4)
    }

    /// The cells of `values`, one a row, in a column `width` wide; numbers
    /// right-aligned in their font.
    private func cells(width: CGFloat) -> [(value: String, rect: CGRect, number: Bool)] {
        values.enumerated().map { index, value in
            let number = value.first?.isNumber == true || value.first == "-"
            return (value, CGRect(x: 10, y: CGFloat(index) * 22, width: width, height: 22), number)
        }
    }

    func testBatchedTextDrawsTheSamePixelsAsCoreText() {
        for width: CGFloat in [260, 90, 40] {
            for truncated in [false, true] {
                let cells = cells(width: width)
                let height = CGFloat(cells.count) * 22
                var lines: [TextLine] = []
                let palette = performDrawing { GridPalette.current() }
                for cell in cells {
                    let request = LineRequest(value: cell.value, truncated: truncated, width: width, number: cell.number)
                    let font = cell.number ? GridFonts.number : GridFonts.cell
                    lines.append(performDrawing { CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette) })
                }
                let reference = render(height: height) { context in
                    for (cell, line) in zip(cells, lines) {
                        let font = cell.number ? GridFonts.number : GridFonts.cell
                        CellPainter.drawText(line, in: cell.rect, font: font, alignment: cell.number ? .trailing : .leading, context: context, ellipsisColor: palette.text)
                    }
                }
                let batch = GlyphBatch()
                let batched = render(height: height) { context in
                    for (cell, line) in zip(cells, lines) {
                        let font = cell.number ? GridFonts.number : GridFonts.cell
                        CellPainter.addText(line, in: cell.rect, font: font, alignment: cell.number ? .trailing : .leading, to: batch, context: context, ellipsisColor: palette.text)
                    }
                    batch.draw(in: context)
                }
                XCTAssertEqual(reference, batched, "width \(width), truncated \(truncated)")
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
        let xs: [CGFloat] = [40, 120, 121, 260.5]
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
        let maker = LineMaker(palette: palette)
        for width: CGFloat in [260, 70, 40] {
            for (index, value) in values.enumerated() {
                let number = index % 3 == 0
                let truncated = index % 4 == 0
                let request = LineRequest(value: value, truncated: truncated, width: width, number: number)
                let available = width - 2 * GridMetrics.cellPadding
                let item = LineMaker.Item(request: request, available: available)
                let ahead = await Task.detached { maker.make(item) }.value
                let font = number ? GridFonts.number : GridFonts.cell
                let here = performDrawing { CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette) }
                XCTAssertEqual(ahead.width, here.width, value)
                XCTAssertEqual(glyphs(ahead.line), glyphs(here.line), value)
                guard available > 2, here.width > available else {
                    XCTAssertNil(ahead.fitted, value)
                    continue
                }
                // As `CellPainter.drawText` cuts it.
                performDrawing {
                    let ellipsis = CellPainter.makeLine("…", font: font, color: palette.text, symbolColor: palette.text)
                    here.fit(available, ellipsis: ellipsis.line)
                }
                XCTAssertEqual(ahead.fittedWidth, here.fittedWidth, value)
                XCTAssertEqual(ahead.fittedLineWidth, here.fittedLineWidth, value)
                XCTAssertEqual(ahead.fitted.map(glyphs), here.fitted.map(glyphs), value)
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
        let light = performDrawing { GridPalette.current() }
        XCTAssertTrue(light.headerBackground === performDrawing { GridPalette.current() }.headerBackground)
        let dark = NSAppearance(named: .darkAqua)!
        var darkPalette: GridPalette?
        dark.performAsCurrentDrawingAppearance { darkPalette = GridPalette.current() }
        XCTAssertFalse(light.rowBackgrounds[0] == darkPalette?.rowBackgrounds[0])
        // A change to the system's colours (the accent colour) makes it again.
        NotificationCenter.default.post(name: NSColor.systemColorsDidChangeNotification, object: nil)
        XCTAssertFalse(light.headerBackground === performDrawing { GridPalette.current() }.headerBackground)
    }

    private func performDrawing<T>(_ body: () -> T) -> T {
        var result: T?
        appearance().performAsCurrentDrawingAppearance { result = body() }
        return result!
    }

    // MARK: Reading ahead

    /// Spins the main run loop until `condition` holds, for work that comes
    /// back to the main actor.
    private func waitUntil(_ condition: () -> Bool, timeout: TimeInterval = 5) {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition(), Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.01))
        }
    }

    func testTilesAreReadAheadOffTheMainThread() {
        let reads = Counter()
        let cache = CellTileCache { rows, columns in
            XCTFail("the draw read \(rows) itself")
            return rows.map { row in TileRow(fieldCount: 3, cells: columns.clamped(to: 0..<3).map { .text("\(row):\($0)", truncated: false) }) }
        }
        _ = cache.cachedCell(row: 0, column: 0, loadedRows: 1_000)
        cache.makeBackgroundFetch = {
            { rows, columns in
                XCTAssertFalse(Thread.isMainThread)
                reads.add()
                return rows.map { row in TileRow(fieldCount: 3, cells: columns.clamped(to: 0..<3).map { .text("\(row):\($0)", truncated: false) }) }
            }
        }
        XCTAssertNil(cache.cachedCell(row: 70, column: 1, loadedRows: 1_000))
        cache.readAhead(rows: 60..<140, columns: 0..<3, loadedRows: 1_000)
        // Asking again while they are being read reads nothing more.
        cache.readAhead(rows: 60..<140, columns: 0..<3, loadedRows: 1_000)
        waitUntil { cache.readAheadCount == 3 }
        XCTAssertEqual(reads.value, 3, "row blocks 0, 1 and 2, once each")
        XCTAssertEqual(cache.cachedCell(row: 70, column: 1, loadedRows: 1_000), .text("70:1", truncated: false))
        XCTAssertEqual(cache.cachedCell(row: 70, column: 5, loadedRows: 1_000), .missing)
        XCTAssertEqual(cache.cell(row: 139, column: 2, loadedRows: 1_000), .text("139:2", truncated: false))
        XCTAssertEqual(cache.fetchCount, 0)
        // Rows not indexed yet aren't read.
        cache.readAhead(rows: 900..<1_100, columns: 0..<3, loadedRows: 1_000)
        waitUntil { cache.readAheadCount == 5 }
        XCTAssertEqual(reads.value, 5, "row blocks 14 and 15, not the ones past row 1,000")
    }

    func testTilesReadAheadBeforeTheCacheIsEmptiedAreDropped() {
        let release = DispatchSemaphore(value: 0)
        let cache = CellTileCache { rows, _ in rows.map { _ in TileRow(fieldCount: 1, cells: [.text("new", truncated: false)]) } }
        cache.makeBackgroundFetch = {
            { rows, _ in
                release.wait()
                return rows.map { _ in TileRow(fieldCount: 1, cells: [.text("old", truncated: false)]) }
            }
        }
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        cache.removeAll()
        release.signal()
        // Let the read come back, and be dropped.
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertEqual(cache.readAheadCount, 0)
        XCTAssertEqual(cache.cell(row: 0, column: 0, loadedRows: 10), .text("new", truncated: false))
    }

    func testAFailedReadAheadIsLeftForTheDraw() {
        let cache = CellTileCache { rows, _ in rows.map { _ in TileRow(fieldCount: 1, cells: [.text("drawn", truncated: false)]) } }
        cache.makeBackgroundFetch = { { _, _ in nil } }
        cache.readAhead(rows: 0..<10, columns: 0..<1, loadedRows: 10)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertNil(cache.cachedCell(row: 0, column: 0, loadedRows: 10))
        XCTAssertEqual(cache.cell(row: 0, column: 0, loadedRows: 10), .text("drawn", truncated: false))
        XCTAssertEqual(cache.fetchCount, 1)
    }

    func testTheGridLaysOutTheRowsAheadOfTheScroll() throws {
        let source = FakeGridSource(rows: 10_000, columns: 3)
        source.text = { row, column in "r\(row)c\(column)" }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 466), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        addTeardownBlock { window.close() }
        let grid = GridContainerView(frame: window.contentLayoutRect)
        window.contentView = grid
        grid.dataSource = source
        grid.setColumnWidths([100, 100, 100])
        grid.layoutSubtreeIfNeeded()
        let gridView = grid.gridView
        let clip = grid.scrollView.contentView
        // Draw the top, then scroll down a little and draw again.
        gridView.cacheDisplay(in: gridView.visibleRect, to: try XCTUnwrap(gridView.bitmapImageRepForCachingDisplay(in: gridView.visibleRect)))
        clip.scroll(to: NSPoint(x: 0, y: 22 * 10))
        grid.scrollView.reflectScrolledClipView(clip)
        gridView.cacheDisplay(in: gridView.visibleRect, to: try XCTUnwrap(gridView.bitmapImageRepForCachingDisplay(in: gridView.visibleRect)))
        // Rows below the visible ones are laid out: their lines are cached
        // before any draw asks for them.
        let below = Int(clip.bounds.maxY / 22) + 2
        let key = LineRequest(value: "r\(below)c0", truncated: false, width: 100, number: false).key
        waitUntil { gridView.lines.contains(key) }
        XCTAssertTrue(gridView.lines.contains(key))
        XCTAssertGreaterThan(gridView.ahead.linesMade, 0)
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
}
