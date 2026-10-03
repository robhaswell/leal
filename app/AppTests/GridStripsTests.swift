import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.0b: the grid and the gutter drawn into strips of rows that
/// scrolling moves (ADR-0011). What the strips show is compared, pixel for
/// pixel, with AppKit drawing the same grid at once (a grid with
/// `allowsStrips` off), through `cacheDisplay`, which draws the strips'
/// backing stores where they are on screen: so a stale, blank or misplaced
/// strip shows in these comparisons as it would on screen. Then the risks
/// ADR-0011 lists: flings, column and window resizes, a move between 1×
/// and 2× displays, VoiceOver frames, the switch to AppKit's drawing for
/// wide grids, the in-cell editor's place, and every path that changes
/// what is drawn.
@MainActor
final class GridStripsTests: XCTestCase {
    private let light = NSAppearance(named: .aqua)!
    private let dark = NSAppearance(named: .darkAqua)!
    /// Wider than the window, so the grid scrolls sideways (but less than
    /// 1.4 times as wide, so it has strips); a column of no width.
    private static let widths: [CGFloat] = [110, 90, 0, 120, 80, 140, 120, 140]
    /// The visible area: 18 rows under the header.
    private static let size = NSSize(width: 640, height: GridMetrics.headerHeight + 18 * 22)

    // MARK: Helpers

    /// A grid in a window that is never shown, with or without strips, in
    /// sRGB (so the strips' backing stores and AppKit's drawing compare
    /// byte for byte), its strips drawn at `scale`.
    private func makeGrid(
        _ source: any GridDataSource,
        strips: Bool,
        appearance: NSAppearance? = nil,
        scale: CGFloat = 2,
        widths: [CGFloat] = widths,
        size: NSSize = size
    ) -> GridContainerView {
        let window = NSWindow(contentRect: NSRect(origin: .zero, size: size), styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.colorSpace = .sRGB
        window.appearance = appearance ?? light
        addTeardownBlock { window.close() }
        let grid = GridContainerView(frame: NSRect(origin: .zero, size: size), allowsStrips: strips)
        grid.scrollView.scrollerStyle = .overlay
        window.contentView = grid
        grid.stripScaleForTesting = scale
        grid.dataSource = source
        grid.setColumnWidths(widths)
        grid.layoutSubtreeIfNeeded()
        let line = GridContainerView.stripLine(visibleWidth: grid.scrollView.contentView.bounds.width, returning: false)
        XCTAssertEqual(grid.strips != nil, strips && grid.gridView.frame.width <= line)
        XCTAssertEqual(grid.gutterStrips != nil, strips)
        return grid
    }

    /// A grid with strips and one drawn by AppKit, showing the same.
    private func makePair(_ source: any GridDataSource, appearance: NSAppearance? = nil, scale: CGFloat = 2, widths: [CGFloat] = widths) -> (strips: GridContainerView, plain: GridContainerView) {
        (makeGrid(source, strips: true, appearance: appearance, scale: scale, widths: widths),
         makeGrid(source, strips: false, appearance: appearance, scale: scale, widths: widths))
    }

    /// `view`, drawn by `cacheDisplay` into an sRGB bitmap at `scale`.
    private func snapshot(_ view: NSView, scale: CGFloat = 2, rect: NSRect? = nil) -> NSBitmapImageRep {
        let rect = rect ?? view.bounds
        let rep = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: Int((rect.width * scale).rounded()),
            pixelsHigh: Int((rect.height * scale).rounded()),
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        )!.retagging(with: .sRGB)!
        rep.size = rect.size
        view.cacheDisplay(in: rect, to: rep)
        if !(view is StripContentView) {
            // A frame later: a strip that draws a row for the first time
            // can ask for ink it spills upwards into the strip above to be
            // drawn, in the next frame (`GridView.spills`). (A grid view's
            // own snapshot waits for that itself.)
            view.cacheDisplay(in: rect, to: rep)
        }
        return rep
    }

    /// Lets AppKit tell views what changed (an appearance, as it does
    /// before the next frame).
    private func settle() {
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))
    }

    private func bytes(_ rep: NSBitmapImageRep) -> Data {
        Data(bytes: rep.bitmapData!, count: rep.bytesPerRow * rep.pixelsHigh)
    }

    /// Fails unless the two bitmaps are the same, saving both for a look
    /// if not. A channel may differ by one level: a translucent colour
    /// (the hatching's, in dark) blended into a strip's backing store and
    /// then into the bitmap rounds once more than when drawn into the
    /// bitmap directly. A stale, blank or misplaced strip, or text or a
    /// mark drawn anywhere else, differs by far more.
    private func assertSame(
        _ drawn: NSBitmapImageRep,
        _ reference: NSBitmapImageRep,
        _ message: @autoclosure () -> String,
        file: StaticString = #filePath,
        line: UInt = #line
    ) {
        guard bytes(drawn) != bytes(reference) else { return }
        var differing = 0
        var bounds = CGRect.null
        if drawn.pixelsWide == reference.pixelsWide, drawn.pixelsHigh == reference.pixelsHigh {
            let a = drawn.bitmapData!
            let b = reference.bitmapData!
            for y in 0..<drawn.pixelsHigh {
                for x in 0..<drawn.pixelsWide {
                    let i = y * drawn.bytesPerRow + x * 4
                    if (0..<4).contains(where: { abs(Int(a[i + $0]) - Int(b[i + $0])) > 1 }) {
                        differing += 1
                        bounds = bounds.union(CGRect(x: x, y: y, width: 1, height: 1))
                    }
                }
            }
            guard differing > 0 else { return }
        }
        let name = "\(Self.self)-\(line)-\(UUID().uuidString.prefix(6))"
        let folder = FileManager.default.temporaryDirectory
        try? drawn.representation(using: .png, properties: [:])?.write(to: folder.appending(path: "\(name)-strips.png"))
        try? reference.representation(using: .png, properties: [:])?.write(to: folder.appending(path: "\(name)-reference.png"))
        XCTFail("\(message()): \(differing) pixels differ, within \(bounds) (pixels); saved as \(folder.path)/\(name)-*.png", file: file, line: line)
    }

    private func assertDifferent(_ a: NSBitmapImageRep, _ b: NSBitmapImageRep, _ message: String, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertNotEqual(bytes(a), bytes(b), message, file: file, line: line)
    }

    private func scroll(_ grid: GridContainerView, to point: NSPoint) {
        let clip = grid.scrollView.contentView
        clip.scroll(to: point)
        grid.scrollView.reflectScrolledClipView(clip)
    }

    private func scroll(_ grids: GridContainerView..., to point: NSPoint) {
        for grid in grids { scroll(grid, to: point) }
    }

    /// Both grids' cells, header, gutter and corner: through the strips in
    /// the first, as AppKit draws them in the second.
    private func assertSameGrid(_ grid: GridContainerView, _ plain: GridContainerView, scale: CGFloat = 2, _ message: @autoclosure () -> String, file: StaticString = #filePath, line: UInt = #line) {
        assertSame(snapshot(grid, scale: scale), reference(plain, scale: scale), message(), file: file, line: line)
    }

    /// A grid drawn by AppKit, its cells drawn again over ten rows more
    /// each way, so that ink spilling into view from rows out of view is
    /// drawn, as when the whole grid is drawn at once. (AppKit draws only
    /// the rows in view, so it would leave that ink out.)
    private func reference(_ plain: GridContainerView, scale: CGFloat = 2) -> NSBitmapImageRep {
        XCTAssertNil(plain.strips)
        let rep = snapshot(plain, scale: scale)
        let clip = plain.scrollView.contentView
        let area = plain.convert(clip.frame, from: plain.scrollView)
        let visible = clip.bounds
        let graphics = NSGraphicsContext(bitmapImageRep: rep)!
        let context = graphics.cgContext
        context.concatenate(context.ctm.inverted())
        context.scaleBy(x: scale, y: scale)
        // The container's coordinates, which go down.
        context.translateBy(x: 0, y: plain.bounds.height)
        context.scaleBy(x: 1, y: -1)
        context.clip(to: area)
        // The grid view's.
        context.translateBy(x: area.minX - visible.minX, y: area.minY - visible.minY)
        context.clip(to: plain.gridView.bounds)
        plain.gridView.drawDirectly(visible.insetBy(dx: 0, dy: -10 * 22).intersection(plain.gridView.bounds), context: context)
        graphics.flushGraphics()
        return rep
    }

    // MARK: Pixels

    /// The strips draw what AppKit draws, pixel for pixel: a selection,
    /// find's marks and its current match, hatched missing cells, skeleton
    /// rows, a column of no width, numbers, other scripts and emoji, values
    /// whose stacked marks spill into the cell below (across a strip's edge
    /// too), and the gutter's numbers and markers; light and dark, at 2×
    /// and 1×, at the top, scrolled down and sideways, and at the skeleton
    /// rows at the end.
    func testTheStripsDrawWhatAppKitDraws() {
        let source = StripSource(rows: 400, loaded: 380)
        let highlighter = StripHighlighter()
        for (appearance, scale) in [(light, 2.0), (dark, 2.0), (light, 1.0), (dark, 1.0)] {
            let (grid, plain) = makePair(source, appearance: appearance, scale: scale)
            for (index, offset) in [NSPoint(x: 0, y: 0), NSPoint(x: 37, y: 22 * 41 + 11), NSPoint(x: 300, y: 22 * 375)].enumerated() {
                for each in [grid, plain] {
                    each.highlighter = highlighter
                    scroll(each, to: offset)
                    let top = each.visibleRows.lowerBound
                    // The selection's cells, and its rows' numbers.
                    each.selection = GridSelection(active: CellPosition(row: top + 2, column: 0), anchor: CellPosition(row: top + 2, column: 0), extent: CellPosition(row: top + 4, column: 3))
                }
                XCTAssertNotNil(grid.strips)
                assertSameGrid(grid, plain, scale: scale, "\(appearance.name.rawValue) at \(scale)×, place \(index)")
            }
        }
    }

    /// Ink that spills past a cell lands where it does when the whole grid
    /// is drawn at once, whichever strip the cell is in: the spilling value
    /// is in each of a strip's four rows in turn, and in the rows either
    /// side of a strip's edge.
    func testInkThatSpillsPastACellIsDrawnInTheNextStrip() {
        for spilling in 0..<8 {
            let source = StripSource(rows: 60)
            source.spillingRow = 8 + spilling
            let (grid, plain) = makePair(source)
            assertSameGrid(grid, plain, "the spilling value in row \(8 + spilling)")
        }
        // Without drawing the rows either side, a strip would lose the
        // spill from the row above it: the marks really cross the edge.
        let source = StripSource(rows: 60)
        source.spillingRow = 11
        let (grid, plain) = makePair(source)
        let edge = CGRect(x: 0, y: 12 * 22, width: 110, height: 6)
        source.spillingRow = nil
        plain.gridView.needsDisplay = true
        assertDifferent(snapshot(grid.gridView, rect: edge), snapshot(plain.gridView, rect: edge), "row 11's marks reach row 12, in the next strip")
    }

    // MARK: Snapshots go through the strips

    /// `cacheDisplay` (tests, screenshots) draws what the strips hold, not
    /// the grid drawn afresh: a strip not redrawn after a change, a strip
    /// missing and a strip out of place all show, in a snapshot of the
    /// whole grid and of the grid view alone.
    func testSnapshotsShowWhatTheStripsHold() throws {
        let source = StripSource(rows: 200)
        let (grid, plain) = makePair(source)
        let before = snapshot(grid)
        assertSame(before, reference(plain), "at first")
        // Every value changes, and nothing tells the grid: the strips still
        // hold the old values (AppKit would draw the new ones).
        source.version = 1
        plain.gridView.needsDisplay = true
        assertSame(snapshot(grid), before, "the strips weren't told")
        assertDifferent(snapshot(grid), reference(plain), "the plain grid draws the new values")
        let visible = grid.gridView.visibleRect
        assertDifferent(snapshot(grid.gridView, rect: visible), snapshot(plain.gridView, rect: visible), "the grid view's own snapshot")
        grid.gridView.needsDisplay = true
        assertSame(snapshot(grid), reference(plain), "once told")

        let strips = try XCTUnwrap(grid.strips)
        let strip = try XCTUnwrap(strips.strip(at: 1))
        strip.isHidden = true
        assertDifferent(snapshot(grid), reference(plain), "a missing strip shows")
        assertDifferent(snapshot(grid.gridView, rect: visible), snapshot(plain.gridView, rect: visible), "in the grid view's snapshot too")
        strip.isHidden = false
        strip.position.y += 1
        assertDifferent(snapshot(grid), reference(plain), "a strip out of place shows")
        assertDifferent(snapshot(grid.gridView, rect: visible), snapshot(plain.gridView, rect: visible), "in the grid view's snapshot too")
        strip.position.y -= 1
        assertSame(snapshot(grid), reference(plain), "back in place")
        // The gutter's strips likewise.
        let number = try XCTUnwrap(grid.gutterStrips?.strip(at: 0))
        number.isHidden = true
        assertDifferent(snapshot(grid), reference(plain), "a missing strip of the gutter shows")
        number.isHidden = false
        // Past the visible rows, the grid view's snapshot draws the rows
        // directly, as AppKit would.
        let whole = grid.gridView.bounds.intersection(CGRect(x: 0, y: 0, width: 900, height: 60 * 22))
        assertSame(snapshot(grid.gridView, rect: whole), snapshot(plain.gridView, rect: whole), "past the visible rows")
    }

    /// The window's snapshot (`just snapshot`, the screenshots) goes
    /// through the strips too.
    func testAWindowSnapshotGoesThroughTheStrips() throws {
        let source = StripSource(rows: 200)
        let (grid, plain) = makePair(source)
        let frame = try XCTUnwrap(grid.window?.contentView?.superview)
        let plainFrame = try XCTUnwrap(plain.window?.contentView?.superview)
        assertSame(snapshot(frame), snapshot(plainFrame), "the window")
        try XCTUnwrap(grid.strips?.strip(at: 2)).isHidden = true
        assertDifferent(snapshot(frame), snapshot(plainFrame), "a missing strip shows in the window's snapshot")
    }

    // MARK: Flings

    /// A fling, as the scroll benchmark and a trackpad give it: steps of
    /// 900 points a frame slowing to 1, down, then back up, a jump (the
    /// scroller dragged), sideways and back. After every step the strips
    /// show what AppKit draws: none stale, blank or out of place. The
    /// selection and find's marks change mid-fling, with strips ahead of the
    /// scroll that show them.
    func testAFlingNeverShowsAStaleOrBlankStrip() throws {
        let source = StripSource(rows: 100_000)
        let highlighter = StripHighlighter()
        let (grid, plain) = makePair(source)
        let strips = try XCTUnwrap(grid.strips)
        var steps: [NSPoint] = []
        var y: CGFloat = 0
        var speed: CGFloat = 900
        while speed >= 1 {
            y += speed
            steps.append(NSPoint(x: 0, y: y))
            speed = (speed * 0.8).rounded(.down)
        }
        speed = 700
        while speed >= 1 {
            y -= speed
            steps.append(NSPoint(x: 0, y: max(0, y)))
            speed = (speed * 0.75).rounded(.down)
        }
        steps += [NSPoint(x: 0, y: 1_000_000), NSPoint(x: 0, y: 1_000_000 + 5.5), NSPoint(x: 120, y: 1_000_010), NSPoint(x: 400, y: 1_000_010), NSPoint(x: 0, y: 999_950)]
        for (index, point) in steps.enumerated() {
            scroll(grid, plain, to: point)
            if index == 10 {
                // Rows from the second visible one to past the strip ahead.
                let top = grid.visibleRows.lowerBound
                for each in [grid, plain] {
                    each.selection = GridSelection(active: CellPosition(row: top + 1, column: 1), anchor: CellPosition(row: top + 1, column: 1), extent: CellPosition(row: top + 30, column: 2))
                }
            }
            if index == 20 {
                for each in [grid, plain] { each.highlighter = highlighter }
                plain.gridView.needsDisplay = true
                grid.gridView.needsDisplay = true
            }
            // Every row in view is in a strip that is placed and shown.
            let visible = grid.scrollView.contentView.bounds
            let covered = strips.placedIndexes.map(strips.rect(ofStrip:)).reduce(CGRect.null) { $0.union($1) }
            XCTAssertTrue(covered.contains(visible.intersection(grid.gridView.bounds)), "step \(index): \(visible) not covered by \(covered)")
            assertSameGrid(grid, plain, "step \(index), at \(point)")
        }
    }

    /// A scroll within the strips placed, or sideways, draws nothing: only
    /// strips coming into view, and a strip's height ahead, are drawn.
    func testAScrollDrawsOnlyTheStripsComingIntoView() throws {
        let source = StripSource(rows: 10_000)
        let grid = makeGrid(source, strips: true)
        let strips = try XCTUnwrap(grid.strips)
        // Settled: the strips in view and one either side.
        scroll(grid, to: NSPoint(x: 0, y: 22 * 100))
        scroll(grid, to: NSPoint(x: 0, y: 22 * 100))
        _ = snapshot(grid)
        let placed = strips.placedIndexes
        let inView = Int((grid.scrollView.contentView.bounds.height / strips.stripHeight).rounded(.up)) + 1
        XCTAssertLessThanOrEqual(placed.count, inView + 2, "\(placed)")
        let drawn = strips.stripsDrawn
        scroll(grid, to: NSPoint(x: 0, y: 22 * 100 + 30))
        scroll(grid, to: NSPoint(x: 260, y: 22 * 100 + 30))
        scroll(grid, to: NSPoint(x: 0, y: 22 * 100 + 30))
        _ = snapshot(grid)
        XCTAssertEqual(strips.stripsDrawn, drawn, "nothing new came into view")
        // Down a screen: the strips that came into view, and no more than
        // `ahead` beyond them.
        scroll(grid, to: NSPoint(x: 0, y: 22 * 120))
        _ = snapshot(grid)
        let newStrips = Int((22 * 20 / strips.stripHeight).rounded(.up)) + 1
        XCTAssertLessThanOrEqual(strips.stripsDrawn - drawn, newStrips + GridStrips.ahead)
        XCTAssertGreaterThan(strips.stripsDrawn, drawn)
        // Spare strips are reused, not made a frame at a time.
        let made = strips.stripsMade
        for step in 1...200 {
            scroll(grid, to: NSPoint(x: 0, y: 22 * 120 + CGFloat(step) * 40))
        }
        XCTAssertLessThanOrEqual(strips.stripsMade - made, GridStrips.ahead + 1)
    }

    /// Selecting a cell redraws that cell's part of its strip, not the
    /// strips (ADR-0011's "selection and find redraw cost").
    func testSelectingACellRedrawsOnlyThatCell() throws {
        let source = StripSource(rows: 1_000)
        let grid = makeGrid(source, strips: true)
        grid.activeCell = CellPosition(row: 3, column: 1)
        _ = snapshot(grid)
        let area = grid.gridView.drawnArea
        grid.activeCell = CellPosition(row: 9, column: 3)
        _ = snapshot(grid)
        let cell = grid.geometry.cellRect(row: 9, column: 3).insetBy(dx: -2, dy: -2)
        // Two cells (the old and the new), and the rows either side of
        // each for spilling ink.
        XCTAssertLessThanOrEqual(grid.gridView.drawnArea - area, 2 * cell.width * (cell.height + 2 * 22) + 1)
    }

    // MARK: Resizing

    /// A column resized, by a step at a time as a drag does: every strip is
    /// drawn again at the new widths. Past 1.5 times the visible width the
    /// grid keeps AppKit's drawing, and goes back to strips only below 1.4
    /// times it.
    func testAColumnResizeRedrawsTheStripsAndWideGridsKeepAppKitsDrawing() {
        let source = StripSource(rows: 1_000)
        let (grid, plain) = makePair(source)
        scroll(grid, plain, to: NSPoint(x: 50, y: 22 * 30))
        for width in [150, 151, 40, 0, 120] as [CGFloat] {
            grid.setWidth(width, ofColumn: 1)
            plain.setWidth(width, ofColumn: 1)
            assertSameGrid(grid, plain, "column 1 at \(width) pt")
        }
        let visible = grid.scrollView.contentView.bounds.width
        let line = (1.5 * visible).rounded(.down)
        let back = (1.4 * visible).rounded(.down)
        XCTAssertEqual(GridContainerView.stripLine(visibleWidth: visible, returning: false), 1.5 * visible)
        // (Column 1 is 120 pt now.)
        let rest = Self.widths.reduce(0, +) - Self.widths[7] - Self.widths[1] + 120
        func setTotal(_ total: CGFloat) {
            for each in [grid, plain] { each.setWidth(total - rest, ofColumn: 7) }
            XCTAssertEqual(grid.gridView.frame.width, total)
        }
        setTotal(line)
        XCTAssertNotNil(grid.strips, "\(line) pt: strips")
        assertSameGrid(grid, plain, "at the line")
        setTotal(line + 1)
        XCTAssertNil(grid.strips, "\(line + 1) pt: AppKit's drawing")
        XCTAssertNil(grid.gridView.strips)
        XCTAssertFalse(grid.gridView.wantsUpdateLayer)
        XCTAssertNotNil(grid.gutterStrips, "the gutter keeps its strips")
        assertSameGrid(grid, plain, "past the line")
        for total in [line, line - 20, back + 1, line + 100] {
            setTotal(total)
            XCTAssertNil(grid.strips, "\(total) pt: still AppKit's drawing")
        }
        setTotal(back)
        XCTAssertNotNil(grid.strips, "\(back) pt: strips again")
        XCTAssertTrue(grid.gridView.wantsUpdateLayer)
        assertSameGrid(grid, plain, "back below the line")
        setTotal(line - 1)
        XCTAssertNotNil(grid.strips, "\(line - 1) pt: still strips")
        assertSameGrid(grid, plain, "near the line")
    }

    /// Resizing the window moves the line: a grid that is too wide for
    /// strips in a narrow window has them in a wide one.
    func testAWindowResizeCanCrossTheLine() throws {
        let source = StripSource(rows: 1_000, columns: 4)
        let widths: [CGFloat] = [250, 250, 250, 250]
        let (grid, plain) = makePair(source, widths: widths)
        let gutter = grid.window!.contentLayoutRect.width - grid.scrollView.contentView.bounds.width
        var hadStrips = grid.strips != nil
        var modes: Set<Bool> = []
        // Visible widths of 747, 687, 647, 687 and 727 pt: strips while
        // 1,000 pt is within 1.5 times it, or once AppKit draws it, 1.4.
        for visible in [747, 687, 647, 687, 727] as [CGFloat] {
            for each in [grid, plain] {
                each.window?.setContentSize(NSSize(width: visible + gutter, height: Self.size.height))
                each.layoutSubtreeIfNeeded()
            }
            XCTAssertEqual(grid.scrollView.contentView.bounds.width, visible)
            let expected = 1_000 <= (hadStrips ? 1.5 : 1.4) * visible
            XCTAssertEqual(grid.strips != nil, expected, "visible width \(visible) pt")
            assertSameGrid(grid, plain, "visible width \(visible) pt")
            hadStrips = grid.strips != nil
            modes.insert(hadStrips)
        }
        XCTAssertEqual(modes, [true, false], "both drawings were used")
    }

    /// In a very wide window the line is 4,096 pt (ADR-0011's), and a grid
    /// goes back to strips below 3,840 pt.
    func testStripsAreNeverWiderThan4096Points() {
        let source = StripSource(rows: 200, columns: 32)
        let size = NSSize(width: 3_200, height: 300)
        // 376 + 31 × 120 = 4,096 pt.
        let grid = makeGrid(source, strips: true, widths: [376] + Array(repeating: 120, count: 31), size: size)
        XCTAssertGreaterThan(1.4 * grid.scrollView.contentView.bounds.width, 4_096)
        XCTAssertEqual(grid.gridView.frame.width, 4_096)
        XCTAssertNotNil(grid.strips, "4,096 pt: strips")
        grid.setWidth(376.5, ofColumn: 0)
        XCTAssertNil(grid.strips, "past 4,096 pt: AppKit's drawing")
        grid.setWidth(121, ofColumn: 0)
        XCTAssertNil(grid.strips, "3,841 pt: still AppKit's drawing")
        grid.setWidth(120, ofColumn: 0)
        XCTAssertNotNil(grid.strips, "3,840 pt: strips again")
    }

    /// A grid opened past the line starts with AppKit's drawing; one between
    /// the hysteresis and the line starts with strips.
    func testTheFirstWidthDecidesByTheLineItself() {
        let source = StripSource(rows: 100, columns: 9)
        let probe = makeGrid(source, strips: true, widths: Array(repeating: 10, count: 9))
        let line = GridContainerView.stripLine(visibleWidth: probe.scrollView.contentView.bounds.width, returning: false)
        let wide = makeGrid(source, strips: true, widths: Array(repeating: (line + 9) / 9, count: 9))
        XCTAssertNil(wide.strips, "just past the line")
        let near = makeGrid(source, strips: true, widths: Array(repeating: (line - 9) / 9, count: 9))
        XCTAssertNotNil(near.strips, "just within it, past the hysteresis")
        let plain = makeGrid(source, strips: false, widths: Array(repeating: (line - 9) / 9, count: 9))
        scroll(near, plain, to: NSPoint(x: 200, y: 22 * 50))
        assertSameGrid(near, plain, "just within the line")
    }

    /// The window resized a step at a time, as a live resize does: taller,
    /// shorter, wider than the columns and narrower. The strips always show
    /// what AppKit draws, and a step that leaves the strips' width alone
    /// draws only the strips coming into view.
    func testALiveWindowResizeKeepsTheStripsRight() throws {
        let source = StripSource(rows: 5_000, columns: 4)
        let widths: [CGFloat] = [110, 90, 120, 80]
        let (grid, plain) = makePair(source, widths: widths)
        scroll(grid, plain, to: NSPoint(x: 0, y: 22 * 200))
        let strips = try XCTUnwrap(grid.strips)
        assertSameGrid(grid, plain, "before")
        var sizes: [NSSize] = []
        for step in 0..<12 { sizes.append(NSSize(width: 640 + CGFloat(step) * 23, height: 422 + CGFloat(step) * 31)) }
        for step in 0..<12 { sizes.append(NSSize(width: 893 - CGFloat(step) * 41, height: 763 - CGFloat(step) * 37)) }
        var widthChanges = 0
        for size in sizes {
            let drawn = strips.stripsDrawn
            let width = strips.stripWidth
            let tall = strips.placedIndexes.count
            for each in [grid, plain] {
                each.window?.setContentSize(size)
                each.layoutSubtreeIfNeeded()
            }
            assertSameGrid(grid, plain, "at \(size)")
            if strips.stripWidth == width {
                XCTAssertLessThanOrEqual(strips.stripsDrawn - drawn, max(0, strips.placedIndexes.count - tall) + GridStrips.ahead + 1, "at \(size)")
            } else {
                widthChanges += 1
            }
        }
        // The strips' width is the grid's rounded up to 256 pt: 24 steps
        // between 400 and 850 pt change it only a few times.
        XCTAssertLessThanOrEqual(widthChanges, 5)
    }

    // MARK: Displays

    /// Moving the window between a 2× and a 1× display (AppKit calls
    /// `viewDidChangeBackingProperties`): every strip is drawn again at the
    /// new scale.
    func testMovingToADisplayOfAnotherScaleRedrawsTheStrips() throws {
        let source = StripSource(rows: 1_000)
        let (grid, plain) = makePair(source, scale: 2)
        scroll(grid, plain, to: NSPoint(x: 0, y: 22 * 10))
        assertSameGrid(grid, plain, scale: 2, "at 2×")
        for scale in [1.0, 2.0, 1.0] as [CGFloat] {
            grid.stripScaleForTesting = scale
            let strips = try XCTUnwrap(grid.strips)
            for index in strips.placedIndexes {
                XCTAssertEqual(strips.strip(at: index)?.contentsScale, scale)
                XCTAssertTrue(strips.strip(at: index)?.needsDisplay() == true, "strip \(index) is drawn again")
            }
            for index in try XCTUnwrap(grid.gutterStrips).placedIndexes {
                XCTAssertEqual(grid.gutterStrips?.strip(at: index)?.contentsScale, scale)
            }
            assertSameGrid(grid, plain, scale: scale, "moved to \(scale)×")
        }
        // Strips that come into view later are at the new scale too.
        scroll(grid, plain, to: NSPoint(x: 0, y: 22 * 300))
        for index in try XCTUnwrap(grid.strips).placedIndexes {
            XCTAssertEqual(grid.strips?.strip(at: index)?.contentsScale, 1)
        }
        assertSameGrid(grid, plain, scale: 1, "scrolled at 1×")
    }

    // MARK: VoiceOver and the editor's place

    /// VoiceOver's frames (4.2) come from the grid view's geometry, which
    /// the strips don't move: each strip is where the grid view says its
    /// rows are, in the window, at the top, scrolled by fractions of a
    /// point, and at the end of a grid of 40M rows (880M points).
    func testStripsLieWhereTheGridSaysItsRowsAre() throws {
        for rows in [1_000, 40_000_000] {
            let source = StripSource(rows: rows)
            let grid = makeGrid(source, strips: true)
            let maxY = grid.gridView.frame.height - grid.scrollView.contentView.bounds.height
            for point in [NSPoint(x: 0, y: 0), NSPoint(x: 13.5, y: 22 * 37 + 7.5), NSPoint(x: 0, y: maxY), NSPoint(x: 0, y: maxY - 1_000.5)] {
                scroll(grid, to: point)
                for strips in [grid.strips, grid.gutterStrips].compactMap({ $0 }) {
                    let content: NSView = strips === grid.strips ? grid.gridView : grid.gutterView
                    for index in strips.placedIndexes {
                        let layer = try XCTUnwrap(strips.strip(at: index))
                        // The strips' view isn't flipped: its coordinates
                        // are its root layer's.
                        XCTAssertFalse(strips.view.isFlipped)
                        let shown = strips.view.convert(layer.convert(layer.bounds, to: strips.view.root), to: nil)
                        let expected = content.convert(strips.rect(ofStrip: index), to: nil)
                        XCTAssertEqual(shown.minX, expected.minX, accuracy: 0.001, "\(rows) rows at \(point), strip \(index)")
                        XCTAssertEqual(shown.minY, expected.minY, accuracy: 0.001, "\(rows) rows at \(point), strip \(index)")
                        XCTAssertEqual(shown.height, expected.height, accuracy: 0.001)
                    }
                }
            }
        }
    }

    /// A cell's frame in the window, as VoiceOver would give it, is where
    /// its text is in the window's snapshot.
    func testACellsFrameIsWhereItsTextIs() throws {
        let source = StripSource(rows: 1_000, columns: 4)
        source.blank = true
        source.edited[CellPosition(row: 52, column: 2)] = "MMMMMMMM"
        let grid = makeGrid(source, strips: true, widths: [110, 90, 120, 80])
        scroll(grid, to: NSPoint(x: 0, y: 22 * 45 + 5))
        let view = try XCTUnwrap(grid.window?.contentView)
        let image = snapshot(view)
        // (The grid's container is flipped, like the bitmap's rows.)
        XCTAssertTrue(view.isFlipped)
        let cell = grid.gridView.convert(grid.geometry.cellRect(row: 52, column: 2), to: view)
        func dark(_ rect: NSRect) -> Int {
            var count = 0
            for y in Int(rect.minY * 2)..<Int(rect.maxY * 2) {
                for x in Int(rect.minX * 2)..<Int(rect.maxX * 2) {
                    if let color = image.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.brightnessComponent < 0.5 { count += 1 }
                }
            }
            return count
        }
        XCTAssertGreaterThan(dark(cell.insetBy(dx: 1, dy: 1)), 50)
        let cells = grid.gridView.convert(grid.gridView.visibleRect, to: view)
        XCTAssertEqual(dark(cells.insetBy(dx: 1, dy: 1)), dark(cell.insetBy(dx: -1, dy: -1)), "no text outside the cell")
    }

    /// The in-cell editor (task 2.5) and the invalid-bytes callout go in
    /// the overlay: above the strips, under the scrollers, at a cell's
    /// place as the grid scrolls, taking clicks only on themselves.
    func testTheOverlaySitsAboveTheStripsAndFollowsTheScroll() throws {
        let source = StripSource(rows: 40_000_000)
        let grid = makeGrid(source, strips: true)
        let strips = try XCTUnwrap(grid.strips)
        let subviews = grid.scrollView.subviews
        let overlayIndex = try XCTUnwrap(subviews.firstIndex(of: grid.overlay))
        let stripsIndex = try XCTUnwrap(subviews.firstIndex(of: strips.view))
        let clipIndex = try XCTUnwrap(subviews.firstIndex(of: grid.scrollView.contentView))
        XCTAssertLessThan(clipIndex, stripsIndex)
        XCTAssertLessThan(stripsIndex, overlayIndex)
        for scroller in [grid.scrollView.verticalScroller, grid.scrollView.horizontalScroller].compactMap({ $0 }) {
            if let index = subviews.firstIndex(of: scroller) { XCTAssertLessThan(overlayIndex, index) }
        }
        XCTAssertTrue(grid.overlay.isHidden, "nothing to show")

        /// The view a click at `point` (in the window) goes to.
        func hit(_ point: NSPoint) -> NSView? {
            let content = grid.window!.contentView!
            return content.hitTest(content.superview!.convert(point, from: nil))
        }
        let editor = NSTextField(string: "editing")
        let cell = CellPosition(row: 39_999_990, column: 3)
        let rect = grid.geometry.cellRect(row: cell.row, column: cell.column)
        let maxY = grid.gridView.frame.height - grid.scrollView.contentView.bounds.height
        scroll(grid, to: NSPoint(x: 100, y: maxY))
        grid.overlay.show(editor, at: rect)
        XCTAssertFalse(grid.overlay.isHidden)
        for point in [NSPoint(x: 100, y: maxY), NSPoint(x: 0, y: maxY - 37.5), NSPoint(x: 150, y: maxY - 101)] {
            scroll(grid, to: point)
            let shown = editor.convert(editor.bounds, to: nil)
            let expected = grid.gridView.convert(rect, to: nil)
            XCTAssertEqual(shown.minX, expected.minX, accuracy: 0.001, "at \(point)")
            XCTAssertEqual(shown.minY, expected.minY, accuracy: 0.001, "at \(point)")
            XCTAssertEqual(shown.size, expected.size)
            // A click on the editor goes to it; elsewhere, to the grid.
            let onEditor = hit(NSPoint(x: expected.midX, y: expected.midY))
            XCTAssertTrue(onEditor?.isDescendant(of: editor) == true, "at \(point): \(String(describing: onEditor)), editor at \(expected), clip at \(grid.scrollView.contentView.convert(grid.scrollView.contentView.bounds, to: nil))")
            // (The clip view's top left corner, in the window, whose y goes
            // up.)
            let cells = grid.scrollView.contentView.convert(grid.scrollView.contentView.bounds, to: nil)
            let elsewhere = NSPoint(x: cells.minX + 10, y: cells.maxY - 10)
            XCTAssertFalse(expected.contains(elsewhere))
            let onCell = hit(elsewhere)
            XCTAssertTrue(onCell === grid.gridView, "at \(point): \(String(describing: onCell))")
        }
        editor.removeFromSuperview()
        XCTAssertTrue(grid.overlay.isHidden)
        XCTAssertNil(grid.overlay.rect(of: editor))
    }

    // MARK: What changes what is drawn

    /// Every way the grid learns that what it shows changed redraws the
    /// strips, those ahead of the scroll included: an edit's rows
    /// (`cellsChanged`), the values or columns read again
    /// (`invalidateContent`: a Reload, Treat As, the Header row), new rows
    /// (`reloadData`), the selection, find's marks, and the appearance.
    func testEveryChangeRedrawsTheStrips() throws {
        let source = StripSource(rows: 2_000, loaded: 500)
        let highlighter = StripHighlighter()
        let (grid, plain) = makePair(source)
        // Rows 480 to 497 in view; from row 500, skeletons (not loaded),
        // which come into view three rows down.
        scroll(grid, plain, to: NSPoint(x: 0, y: 22 * 480))
        assertSameGrid(grid, plain, "at first")
        let below = grid.visibleRows.upperBound + 1
        func check(_ what: String, _ change: (GridContainerView) -> Void) {
            change(grid)
            change(plain)
            assertSameGrid(grid, plain, what)
            // And the strip ahead of the scroll, once it comes into view.
            let top = grid.scrollView.contentView.bounds.origin
            scroll(grid, plain, to: NSPoint(x: top.x, y: top.y + 3 * 22))
            assertSameGrid(grid, plain, "\(what), scrolled")
            scroll(grid, plain, to: top)
        }
        check("an edit") { each in
            source.edited[CellPosition(row: grid.visibleRows.lowerBound + 3, column: 0)] = "edited"
            source.edited[CellPosition(row: below, column: 3)] = "edited below"
            each.cellsChanged(rows: (grid.visibleRows.lowerBound + 3)..<(below + 1))
        }
        check("values read again") { each in
            source.version += 1
            each.invalidateContent()
        }
        check("rows read") { each in
            source.loadedRowCount = 2_000
            each.reloadData()
        }
        check("the selection, to past the strip ahead") { each in
            each.selection = GridSelection(active: CellPosition(row: grid.visibleRows.lowerBound + 1, column: 0), anchor: CellPosition(row: grid.visibleRows.lowerBound + 1, column: 0), extent: CellPosition(row: below + 6, column: 4))
        }
        check("find's marks") { each in
            each.highlighter = highlighter
        }
        check("dark") { each in
            each.window?.appearance = dark
            settle()
        }
        check("light") { each in
            each.window?.appearance = light
            settle()
        }
    }

    /// An edit's rows are redrawn without being told the grid's other rows
    /// changed: without `cellsChanged`, the strips keep the old value.
    func testAnEditNeedsCellsChanged() throws {
        let source = StripSource(rows: 200)
        let (grid, plain) = makePair(source)
        assertSameGrid(grid, plain, "before")
        source.edited[CellPosition(row: 5, column: 0)] = "edited"
        plain.cellsChanged(rows: 5..<6)
        assertDifferent(snapshot(grid), reference(plain), "not told")
        grid.cellsChanged(rows: 5..<6)
        assertSameGrid(grid, plain, "told")
    }
}

// MARK: Sources

/// Values for the strips' tests: names, numbers, notes cut short, other
/// scripts and emoji, a two-colour value, short ragged rows (hatched), a
/// value whose stacked marks spill into the cell below, rows not loaded,
/// and gutter markers. `version` changes every value; `edited` overrides
/// cells.
@MainActor
private final class StripSource: GridDataSource {
    var rowCount: Int
    var loadedRowCount: Int
    var columnCount: Int
    var version = 0
    var edited: [CellPosition: String] = [:]
    /// Every row whose number ends in 3 or 9 (or only this one) has the
    /// spilling value in column 0.
    var spillingRow: Int?
    /// Only the edited cells have text.
    var blank = false

    init(rows: Int, loaded: Int? = nil, columns: Int = 8) {
        rowCount = rows
        loadedRowCount = loaded ?? rows
        columnCount = columns
    }

    func headerTitle(column: Int) -> HeaderTitle { HeaderTitle(text: "Column \(column)", style: .name) }
    func isNumeric(column: Int) -> Bool { column == 1 }
    func prepare(rows: Range<Int>, columns: Range<Int>) {}
    func rowHasMarker(_ row: Int) -> Bool { row % 5 == 0 }
    func isHatched(row: Int, column: Int) -> Bool { row % 17 == 6 }

    func cell(row: Int, column: Int) -> GridCell {
        guard row < loadedRowCount else { return .notLoaded }
        if let value = edited[CellPosition(row: row, column: column)] { return .text(value, truncated: false) }
        if blank { return .text("", truncated: false) }
        if row % 17 == 6, column >= 2 { return .missing }
        let marks = "\u{0335}\u{0327}\u{0322}\u{031B}\u{031B}\u{031E}\u{032B}\u{0317}\u{0317}\u{0349}\u{032E}\u{0348}\u{031D}\u{032E}\u{0316}\u{031C}\u{032D}\u{0332}\u{0339}\u{0319}\u{033C}"
        let spills = spillingRow.map { $0 == row } ?? (row % 10 == 3 || row % 10 == 9)
        let suffix = version == 0 ? "" : " v\(version)"
        switch column {
        case 0 where spills: return .text("Z\(marks)", truncated: false)
        case 1: return .text("\(row * 37 + version).5", truncated: false)
        case 3: return .text("SKU-\(60_000 + row) on a long note\(suffix)", truncated: row % 7 == 2)
        case 4 where row % 9 == 4: return .text("a\tb", truncated: false)
        case 4: return .text(row % 2 == 0 ? "서울\(suffix)" : "東京 👍🏽\(suffix)", truncated: false)
        case 5: return .text("שלום עולם\(suffix)", truncated: false)
        default: return .text("Name \(row)\(suffix)", truncated: false)
        }
    }
}

/// Find's marks: in column 3, every eleventh row; row 13 is the current
/// match.
@MainActor
private final class StripHighlighter: GridHighlighter {
    func prepareHighlights(rows: Range<Int>, columns: Range<Int>) {}

    func highlight(row: Int, column: Int) -> CellHighlight? {
        guard column == 3, row % 11 == 2 else { return nil }
        return CellHighlight(ranges: [NSRange(location: 0, length: 4)], isCurrent: row == 13)
    }
}

/// The strips in a document's window, against the real core: every path
/// that changes the values shown redraws them (task 2.0b, item 7). After
/// each, what the strips hold (a snapshot, which draws their backing
/// stores) is what the grid draws now.
@MainActor
final class GridStripsDocumentTests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-strips-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let environment = DocumentEnvironment(
            scheduler: try Scheduler(),
            temp: TempLocations(
                scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
                recordsDir: directory.appending(path: "records").path(percentEncoded: false)
            )
        )
        savedEnvironment = CSVDocument.environment
        CSVDocument.environment = { environment }
    }

    override func tearDown() async throws {
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    private func waitUntil(_ what: String, timeout: TimeInterval = 20, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting until \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// The grid view's visible part as the strips hold it, and as the grid
    /// draws it now, in the same kind of bitmap.
    private func strips(_ grid: GridContainerView) -> (held: Data, now: Data) {
        let view = grid.gridView
        let rect = view.visibleRect
        func bitmap() -> NSBitmapImageRep {
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(rect.width * 2), pixelsHigh: Int(rect.height * 2),
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
            )!.retagging(with: .sRGB)!
            rep.size = rect.size
            return rep
        }
        let held = bitmap()
        view.cacheDisplay(in: rect, to: held)
        let now = bitmap()
        let graphics = NSGraphicsContext(bitmapImageRep: now)!
        let context = graphics.cgContext
        context.concatenate(context.ctm.inverted())
        context.scaleBy(x: 2, y: 2)
        context.translateBy(x: 0, y: rect.height)
        context.scaleBy(x: 1, y: -1)
        context.translateBy(x: -rect.minX, y: -rect.minY)
        view.drawDirectly(rect, context: context)
        graphics.flushGraphics()
        func data(_ rep: NSBitmapImageRep) -> Data { Data(bytes: rep.bitmapData!, count: rep.bytesPerRow * rep.pixelsHigh) }
        return (data(held), data(now))
    }

    func testEveryWayTheValuesChangeRedrawsTheStrips() async throws {
        var text = "id;name,city;country,amount\n"
        for row in 0..<300 { text += "\(row);n\(row),c\(row);k\(row),\(row * 3).25\n" }
        let url = directory.appending(path: "values.csv")
        try Data(text.utf8).write(to: url)
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let model = try XCTUnwrap(document.model)
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        window.colorSpace = .sRGB
        window.appearance = NSAppearance(named: .aqua)
        let grid = controller.content.grid
        grid.stripScaleForTesting = 2
        try await waitUntil("indexed") { model.isIndexComplete }
        controller.content.view.layoutSubtreeIfNeeded()
        let gridStrips = try XCTUnwrap(grid.strips)
        var (held, now) = strips(grid)
        XCTAssertEqual(held, now, "at first")

        /// After `change`, the strips are redrawn, and hold what the grid
        /// draws once the file is read again.
        func check(_ what: String, _ change: () throws -> Void) async throws {
            let before = held
            try change()
            try await waitUntil("\(what): read again") { model.isIndexComplete }
            controller.content.view.layoutSubtreeIfNeeded()
            (held, now) = strips(grid)
            XCTAssertNotEqual(held, before, "\(what): the values shown changed")
            XCTAssertEqual(held, now, "\(what): the strips hold what the grid draws")
        }
        try await check("the Header row") { model.setHeaderRow(false) }
        try await check("Treat As") { model.treatAs(.semicolon) }
        try await check("Reload") {
            try Data(text.replacingOccurrences(of: ";n", with: ";N").utf8).write(to: url)
            try model.reload()
        }
        // An edit's rows (SEAM(2.5)): the strips showing them are drawn
        // again; the others aren't.
        _ = strips(grid)
        let placed = gridStrips.placedIndexes
        XCTAssertGreaterThan(placed.count, 4)
        let index = placed[1]
        let first = index * GridStrips.rows + 1
        model.cellsChanged(rows: first..<(first + 1))
        XCTAssertEqual(gridStrips.strip(at: index)?.needsDisplay(), true, "the edited row's strip")
        XCTAssertEqual(gridStrips.strip(at: placed[3])?.needsDisplay(), false, "another strip")
        document.close()
    }
}
