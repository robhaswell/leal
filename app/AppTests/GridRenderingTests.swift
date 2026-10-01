import AppKit
import XCTest

@testable import Leal

/// A data source with `rows` rows whose cells all read "MMMM", so any two
/// screens of it look the same apart from the row numbers.
@MainActor
final class FakeGridSource: GridDataSource {
    var rowCount: Int
    var loadedRowCount: Int
    var columnCount: Int
    var text: (Int, Int) -> String = { _, _ in "MMMM" }
    private(set) var prepared: [(Range<Int>, Range<Int>)] = []

    init(rows: Int, columns: Int, loaded: Int? = nil) {
        rowCount = rows
        loadedRowCount = loaded ?? rows
        columnCount = columns
    }

    func headerTitle(column: Int) -> HeaderTitle {
        HeaderTitle(text: "col\(column)", style: .name)
    }

    func isNumeric(column: Int) -> Bool { column == 1 }

    func cell(row: Int, column: Int) -> GridCell {
        row < loadedRowCount ? .text(text(row, column), truncated: false) : .notLoaded
    }

    func prepare(rows: Range<Int>, columns: Range<Int>) {
        prepared.append((rows, columns))
    }
}

/// Draws the grid offscreen and looks at the pixels.
@MainActor
final class GridRenderingTests: XCTestCase {
    /// A grid in a window that is never shown, with a visible area whose
    /// height is a whole number of rows.
    private func makeGrid(source: FakeGridSource, size: NSSize = NSSize(width: 600, height: 26 + 20 * 22)) -> (NSWindow, GridContainerView) {
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.appearance = NSAppearance(named: .aqua)
        let grid = GridContainerView(frame: NSRect(origin: .zero, size: size))
        grid.scrollView.scrollerStyle = .overlay
        window.contentView = grid
        grid.dataSource = source
        grid.setColumnWidths(Array(repeating: 100, count: source.columnCount))
        grid.layoutSubtreeIfNeeded()
        addTeardownBlock { window.close() }
        return (window, grid)
    }

    private func render(_ view: NSView, _ rect: NSRect? = nil) throws -> NSBitmapImageRep {
        let rect = rect ?? view.bounds
        view.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: rect))
        view.cacheDisplay(in: rect, to: rep)
        return rep
    }

    /// Pixels in `rect` (in the image's points, origin top left) darker
    /// than `below`: text.
    private func darkPixels(_ rep: NSBitmapImageRep, in rect: NSRect, below: CGFloat = 0.5) -> Int {
        let scale = CGFloat(rep.pixelsWide) / rep.size.width
        var count = 0
        for y in Int(rect.minY * scale)..<Int(rect.maxY * scale) {
            for x in Int(rect.minX * scale)..<Int(rect.maxX * scale) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.brightnessComponent < below {
                    count += 1
                }
            }
        }
        return count
    }

    func testCellsDrawTheirText() throws {
        let source = FakeGridSource(rows: 100, columns: 3)
        let (_, grid) = makeGrid(source: source)
        let image = try render(grid.scrollView)
        // The first cell of the first row has text; the space right of the
        // last column has none.
        XCTAssertGreaterThan(darkPixels(image, in: NSRect(x: 0, y: 0, width: 100, height: 22)), 20)
        XCTAssertEqual(darkPixels(image, in: NSRect(x: 320, y: 0, width: 200, height: 22)), 0)
        XCTAssertFalse(source.prepared.isEmpty)
    }

    func testSkeletonRowsHaveNoText() throws {
        let source = FakeGridSource(rows: 100, columns: 3, loaded: 5)
        let (_, grid) = makeGrid(source: source)
        let image = try render(grid.scrollView)
        XCTAssertGreaterThan(darkPixels(image, in: NSRect(x: 0, y: 4 * 22, width: 300, height: 22)), 20)
        XCTAssertEqual(darkPixels(image, in: NSRect(x: 0, y: 5 * 22, width: 300, height: 22)), 0)
        // The gutter numbers only the loaded rows.
        let gutter = try render(grid.gutterView)
        // (Its numbers are in the secondary, grey, text colour.)
        XCTAssertGreaterThan(darkPixels(gutter, in: NSRect(x: 0, y: 4 * 22, width: gutter.size.width, height: 22), below: 0.8), 5)
        XCTAssertEqual(darkPixels(gutter, in: NSRect(x: 0, y: 5 * 22, width: gutter.size.width - 2, height: 22), below: 0.8), 0)
    }

    /// ADR-0001: a plain tall document view draws its last rows correctly,
    /// at 1M rows (22M points down) and at 40M (880M points, about the 4 GiB
    /// limit). The last screen must look exactly like the first.
    func testTheLastRowsOfAVeryTallGridDrawLikeTheFirst() throws {
        for rows in [1_000_000, 40_000_000] {
            let source = FakeGridSource(rows: rows, columns: 4)
            let (_, grid) = makeGrid(source: source)
            let clip = grid.scrollView.contentView
            XCTAssertEqual(grid.gridView.frame.height, CGFloat(rows) * 22)
            let top = try render(grid.scrollView)
            let maxY = grid.gridView.frame.height - clip.bounds.height
            clip.scroll(to: NSPoint(x: 0, y: maxY))
            grid.scrollView.reflectScrolledClipView(clip)
            XCTAssertEqual(grid.visibleRows, (rows - 20)..<rows)
            XCTAssertEqual(grid.gutterView.offsetY, maxY)
            let bottom = try render(grid.scrollView)
            XCTAssertEqual(top.tiffRepresentation, bottom.tiffRepresentation, "\(rows) rows: the last screen differs from the first")
            XCTAssertGreaterThan(darkPixels(bottom, in: NSRect(x: 0, y: 19 * 22, width: 100, height: 22)), 20)
        }
    }

    func testTheHeaderAndGutterFollowTheScroll() throws {
        let source = FakeGridSource(rows: 1_000, columns: 20)
        let (_, grid) = makeGrid(source: source)
        let clip = grid.scrollView.contentView
        clip.scroll(to: NSPoint(x: 250, y: 22 * 300))
        grid.scrollView.reflectScrolledClipView(clip)
        XCTAssertEqual(grid.headerView.offsetX, 250)
        XCTAssertEqual(grid.gutterView.offsetY, 22 * 300)
        XCTAssertEqual(grid.visibleRows, 300..<320)
    }

    func testKeysMoveTheActiveCellAndKeepItInView() {
        let source = FakeGridSource(rows: 1_000, columns: 20)
        let (_, grid) = makeGrid(source: source)
        grid.activeCell = CellPosition(row: 0, column: 0)
        grid.move(.down)
        XCTAssertEqual(grid.activeCell, CellPosition(row: 1, column: 0))
        XCTAssertEqual(grid.gutterView.activeRow, 1)
        grid.move(.lastRow)
        XCTAssertEqual(grid.activeCell, CellPosition(row: 999, column: 0))
        XCTAssertTrue(grid.visibleRows.contains(999))
        grid.move(.lastColumn)
        XCTAssertEqual(grid.activeCell?.column, 19)
        XCTAssertGreaterThan(grid.headerView.offsetX, 0)
        grid.move(.firstRow)
        XCTAssertTrue(grid.visibleRows.contains(0))
    }

    /// DESIGN §3.10 rule 5: ⌘↓ before the index is complete goes to the
    /// estimated last row, shows the pill over skeleton rows, and moves to
    /// the real last row once the index is complete.
    func testJumpingPastTheIndexWaitsForTheRealLastRow() {
        let source = FakeGridSource(rows: 10_000, columns: 3, loaded: 500)
        var complete = false
        let (_, grid) = makeGrid(source: source)
        grid.isIndexComplete = { complete }
        grid.activeCell = CellPosition(row: 0, column: 0)
        grid.move(.lastRow)
        XCTAssertTrue(grid.isJumpingToEnd)
        XCTAssertEqual(grid.activeCell?.row, 9_999)
        XCTAssertFalse(grid.pill.isHidden)
        // The index finds fewer rows than estimated.
        source.rowCount = 9_000
        source.loadedRowCount = 9_000
        complete = true
        grid.reloadData()
        XCTAssertFalse(grid.isJumpingToEnd)
        XCTAssertEqual(grid.activeCell?.row, 8_999)
        XCTAssertTrue(grid.visibleRows.contains(8_999))
        XCTAssertTrue(grid.pill.isHidden)
    }

    /// A click or a scroll after ⌘↓ drops the pending jump: finishing the
    /// index then leaves the active cell where the user put it.
    func testAClickOrScrollAfterJumpingDropsTheJump() {
        let source = FakeGridSource(rows: 10_000, columns: 3, loaded: 500)
        var complete = false
        let (_, grid) = makeGrid(source: source)
        grid.isIndexComplete = { complete }
        grid.activeCell = CellPosition(row: 0, column: 0)
        grid.move(.lastRow)
        XCTAssertTrue(grid.isJumpingToEnd)
        grid.select(CellPosition(row: 3, column: 1))
        XCTAssertFalse(grid.isJumpingToEnd)
        complete = true
        source.rowCount = 9_000
        source.loadedRowCount = 9_000
        grid.reloadData()
        XCTAssertEqual(grid.activeCell, CellPosition(row: 3, column: 1))

        complete = false
        grid.move(.lastRow)
        XCTAssertTrue(grid.isJumpingToEnd)
        grid.scrollView.onScrollInput?()
        XCTAssertFalse(grid.isJumpingToEnd)
        grid.move(.down)
        XCTAssertFalse(grid.isJumpingToEnd)
    }

    /// A window closed mid-gesture never sends the gesture's end; the scroll
    /// view ends it when it leaves the window, once.
    func testAGestureEndsWhenTheGridLeavesItsWindow() {
        let source = FakeGridSource(rows: 10, columns: 3)
        let (window, grid) = makeGrid(source: source)
        var gestures: [Bool] = []
        grid.onGesture = { gestures.append($0) }
        grid.scrollView.setGesture(true)
        grid.scrollView.setGesture(true)
        XCTAssertTrue(grid.scrollView.isInGesture)
        window.contentView = nil
        XCTAssertEqual(gestures, [true, false])
        XCTAssertFalse(grid.scrollView.isInGesture)
    }

    func testDraggingAColumnEdgeResizesIt() {
        let source = FakeGridSource(rows: 10, columns: 3)
        let (_, grid) = makeGrid(source: source)
        var resized: [(Int, CGFloat)] = []
        grid.onColumnResized = { resized.append(($0, $1)) }
        grid.headerView.onResize?(1, 150)
        XCTAssertEqual(grid.geometry.widths, [100, 150, 100])
        XCTAssertEqual(grid.gridView.geometry.offsets.last, 350)
        XCTAssertEqual(resized.first?.1, 150)
        grid.fittingWidth = { _ in 42 }
        grid.headerView.onFit?(0)
        XCTAssertEqual(grid.geometry.widths[0], 42)
    }
}
