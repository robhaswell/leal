import AppKit
import LealFFI
import XCTest

@testable import Leal

/// A highlighter that marks one cell, for drawing tests.
@MainActor
private final class FakeHighlighter: GridHighlighter {
    var cells: [CellPosition: CellHighlight] = [:]
    private(set) var prepared = 0

    func prepareHighlights(rows: Range<Int>, columns: Range<Int>) {
        prepared += 1
    }

    func highlight(row: Int, column: Int) -> CellHighlight? {
        cells[CellPosition(row: row, column: column)]
    }
}

/// Task 1.8's grid work with fake sources: the selection (click,
/// Shift-click, drag, ⌘A, row numbers, Shift with the move keys), Go to Row
/// past the indexed rows, find's highlights, and the words and menus.
@MainActor
final class SelectionTests: XCTestCase {
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

    private func cell(_ row: Int, _ column: Int) -> CellPosition {
        CellPosition(row: row, column: column)
    }

    // MARK: The selection model

    func testASelectionIsTheRectangleBetweenItsCorners() {
        var selection = GridSelection(cell(5, 3))
        XCTAssertFalse(selection.isRange)
        XCTAssertEqual(selection.rows, 5...5)
        selection = selection.extended(to: cell(2, 6))
        XCTAssertEqual(selection.active, cell(5, 3), "the active cell stays")
        XCTAssertEqual(selection.rows, 2...5)
        XCTAssertEqual(selection.columns, 3...6)
        XCTAssertTrue(selection.contains(row: 2, column: 6))
        XCTAssertFalse(selection.contains(row: 1, column: 6))
        XCTAssertFalse(selection.contains(row: 3, column: 2))

        let all = GridSelection.all(rows: 100, columns: 4, active: cell(7, 1))
        XCTAssertEqual(all.rows, 0...99)
        XCTAssertEqual(all.columns, 0...3)
        XCTAssertEqual(all.active, cell(7, 1))
        XCTAssertTrue(all.throughLastRow)
        // The index found more rows than estimated: ⌘A still runs to the end.
        XCTAssertEqual(all.clamped(rows: 120, columns: 4)?.rows, 0...119)
        XCTAssertEqual(all.clamped(rows: 50, columns: 4)?.rows, 0...49)
        XCTAssertEqual(all.clamped(rows: 50, columns: 4)?.active, cell(7, 1))
        XCTAssertNil(all.clamped(rows: 0, columns: 4))

        let row = GridSelection.row(9, columns: 5, column: 2)
        XCTAssertEqual(row.rows, 9...9)
        XCTAssertEqual(row.columns, 0...4)
        XCTAssertEqual(row.active, cell(9, 2))
    }

    // MARK: Mouse and keys

    func testClicksShiftClicksAndDragsSelectCells() {
        let source = FakeGridSource(rows: 1_000, columns: 5)
        let (_, grid) = makeGrid(source: source)
        var changes = 0
        grid.onSelectionChanged = { changes += 1 }
        grid.gridView.onClick?(cell(3, 1))
        XCTAssertEqual(grid.selection, GridSelection(cell(3, 1)))
        grid.gridView.onExtend?(cell(6, 3))
        XCTAssertEqual(grid.selection?.rows, 3...6)
        XCTAssertEqual(grid.selection?.columns, 1...3)
        XCTAssertEqual(grid.activeCell, cell(3, 1))
        XCTAssertEqual(grid.gutterView.selectedRows, 3...6)
        // A drag past the bottom edge selects to the nearest cell there.
        XCTAssertEqual(grid.gridView.cell(at: NSPoint(x: 9_999, y: 1e9), clamped: true), cell(999, 4))
        XCTAssertNil(grid.gridView.cell(at: NSPoint(x: 9_999, y: 10)))
        // A plain click collapses it.
        grid.gridView.onClick?(cell(0, 0))
        XCTAssertFalse(grid.selection?.isRange ?? true)
        XCTAssertNil(grid.gutterView.selectedRows)
        XCTAssertEqual(changes, 3)
    }

    func testShiftWithTheMoveKeysExtendsTheSelection() {
        let source = FakeGridSource(rows: 1_000, columns: 5)
        let (_, grid) = makeGrid(source: source)
        grid.select(cell(10, 1))
        grid.gridView.doCommand(by: #selector(NSResponder.moveDownAndModifySelection(_:)))
        grid.gridView.doCommand(by: #selector(NSResponder.moveDownAndModifySelection(_:)))
        grid.gridView.doCommand(by: #selector(NSResponder.moveRightAndModifySelection(_:)))
        XCTAssertEqual(grid.selection?.rows, 10...12)
        XCTAssertEqual(grid.selection?.columns, 1...2)
        XCTAssertEqual(grid.activeCell, cell(10, 1))
        // ⇧⌘↓ to the last row, scrolled into view; ⇧⌘← to the first column.
        grid.gridView.doCommand(by: #selector(NSResponder.moveToEndOfDocumentAndModifySelection(_:)))
        XCTAssertEqual(grid.selection?.rows, 10...999)
        XCTAssertTrue(grid.visibleRows.contains(999))
        grid.gridView.doCommand(by: #selector(NSResponder.moveToBeginningOfLineAndModifySelection(_:)))
        XCTAssertEqual(grid.selection?.columns, 0...1)
        // Up past the active cell turns the rectangle round.
        grid.gridView.doCommand(by: #selector(NSResponder.moveToBeginningOfDocumentAndModifySelection(_:)))
        XCTAssertEqual(grid.selection?.rows, 0...10)
        // A plain move collapses it and moves the active cell.
        grid.gridView.doCommand(by: #selector(NSResponder.moveDown(_:)))
        XCTAssertEqual(grid.selection, GridSelection(cell(11, 1)))
        // Page Down with Shift.
        grid.gridView.doCommand(by: #selector(NSResponder.pageDownAndModifySelection(_:)))
        XCTAssertEqual(grid.selection?.rows, 11...30)
    }

    func testSelectAllKeepsTheActiveCellAndRowNumbersSelectRows() {
        let source = FakeGridSource(rows: 1_000, columns: 5, loaded: 100)
        let (_, grid) = makeGrid(source: source)
        grid.select(cell(4, 2))
        XCTAssertTrue(grid.gridView.validateMenuItem(NSMenuItem(title: "", action: #selector(NSText.selectAll(_:)), keyEquivalent: "")))
        grid.gridView.selectAll(nil)
        XCTAssertEqual(grid.selection?.rows, 0...999)
        XCTAssertEqual(grid.selection?.columns, 0...4)
        XCTAssertEqual(grid.activeCell, cell(4, 2))
        XCTAssertTrue(grid.visibleRows.contains(4), "Select All doesn't scroll")
        XCTAssertTrue(grid.selection?.throughLastRow ?? false)
        // Indexing finds more rows than the estimate: still every row.
        source.rowCount = 1_200
        source.loadedRowCount = 1_200
        grid.reloadData()
        XCTAssertEqual(grid.selection?.rows, 0...1_199)

        grid.gutterView.onClick?(7)
        XCTAssertEqual(grid.selection, .row(7, columns: 5, column: 2))
        grid.gutterView.onExtend?(3)
        XCTAssertEqual(grid.selection?.rows, 3...7)
        XCTAssertEqual(grid.selection?.columns, 0...4)
        XCTAssertEqual(grid.activeCell, cell(7, 2))
    }

    func testCopyIsOnlyOfferedWithASelection() {
        let source = FakeGridSource(rows: 10, columns: 3)
        let (_, grid) = makeGrid(source: source)
        var copies = 0
        grid.onCopy = { copies += 1 }
        let item = NSMenuItem(title: "", action: #selector(NSText.copy(_:)), keyEquivalent: "")
        grid.activeCell = nil
        XCTAssertFalse(grid.gridView.validateMenuItem(item))
        grid.select(cell(1, 1))
        XCTAssertTrue(grid.gridView.validateMenuItem(item))
        grid.gridView.copy(nil)
        XCTAssertEqual(copies, 1)
    }

    // MARK: Go to Row (DESIGN §3.10 rule 5)

    func testGoToARowPastTheIndexWaitsForIt() {
        let source = FakeGridSource(rows: 10_000, columns: 3, loaded: 500)
        var complete = false
        let (_, grid) = makeGrid(source: source)
        grid.isIndexComplete = { complete }
        grid.select(cell(0, 1))
        // Within the indexed rows: at once.
        grid.goTo(row: 250)
        XCTAssertEqual(grid.activeCell, cell(250, 1))
        XCTAssertNil(grid.pendingJump)
        // Past them: the grid scrolls to where the row should be and waits.
        grid.goTo(row: 7_000)
        XCTAssertEqual(grid.pendingJump, .row(7_000))
        XCTAssertNil(grid.activeCell)
        XCTAssertTrue(grid.visibleRows.contains(7_000))
        XCTAssertFalse(grid.pill.isHidden)
        XCTAssertEqual(GridStrings.pill(jump: .row(7_000), row: 500, of: 10_000), "Reaching row 7,001… row 500 of about 10,000")
        source.loadedRowCount = 6_000
        grid.reloadData()
        XCTAssertEqual(grid.pendingJump, .row(7_000), "not there yet")
        source.loadedRowCount = 8_000
        grid.reloadData()
        XCTAssertNil(grid.pendingJump)
        XCTAssertEqual(grid.activeCell, cell(7_000, 1))
        XCTAssertTrue(grid.visibleRows.contains(7_000))

        // A row past the end of a file that turns out shorter: its last row.
        grid.goTo(row: 9_500)
        XCTAssertEqual(grid.pendingJump, .row(9_500))
        source.rowCount = 9_000
        source.loadedRowCount = 9_000
        complete = true
        grid.reloadData()
        XCTAssertNil(grid.pendingJump)
        XCTAssertEqual(grid.activeCell?.row, 8_999)
        // Once complete, a row past the end goes straight to the last.
        grid.goTo(row: 20_000)
        XCTAssertEqual(grid.activeCell?.row, 8_999)
    }

    func testScrollingDropsAPendingGoToRow() {
        let source = FakeGridSource(rows: 10_000, columns: 3, loaded: 500)
        let (_, grid) = makeGrid(source: source)
        grid.isIndexComplete = { false }
        grid.goTo(row: 7_000)
        grid.scrollView.onScrollInput?()
        XCTAssertNil(grid.pendingJump)
        source.loadedRowCount = 8_000
        grid.reloadData()
        XCTAssertNil(grid.activeCell)
    }

    func testRowNumbersAreReadAsTheGutterShowsThem() {
        XCTAssertEqual(FindText.rowNumber(from: "12"), 12)
        XCTAssertEqual(FindText.rowNumber(from: " 1,000,000 "), 1_000_000)
        XCTAssertEqual(FindText.rowNumber(from: "1 000"), 1_000)
        XCTAssertNil(FindText.rowNumber(from: "0"))
        XCTAssertNil(FindText.rowNumber(from: "-3"))
        XCTAssertNil(FindText.rowNumber(from: "abc"))
        XCTAssertNil(FindText.rowNumber(from: ""))
    }

    // MARK: Find's highlights (mockup 04a)

    func testHighlightsAreDrawnBehindTheMatchAndTheCurrentOneReplacesTheRing() throws {
        let source = FakeGridSource(rows: 100, columns: 3)
        source.text = { row, column in row == 2 && column == 0 ? "xxMarlowxx" : "plain" }
        let (_, grid) = makeGrid(source: source)
        let highlighter = FakeHighlighter()
        highlighter.cells[cell(2, 0)] = CellHighlight(ranges: [NSRange(location: 2, length: 6)], isCurrent: false)
        grid.highlighter = highlighter
        grid.select(cell(5, 2))
        let image = try render(grid.scrollView)
        XCTAssertGreaterThan(highlighter.prepared, 0)
        XCTAssertGreaterThan(yellowPixels(image, in: NSRect(x: 0, y: 2 * 22, width: 100, height: 22)), 40)
        XCTAssertEqual(yellowPixels(image, in: NSRect(x: 0, y: 3 * 22, width: 300, height: 22)), 0)
        // The current match: orange outline, and no accent ring on it.
        grid.select(cell(2, 0))
        highlighter.cells[cell(2, 0)] = CellHighlight(ranges: [NSRange(location: 2, length: 6)], isCurrent: true)
        grid.gridView.needsDisplay = true
        let current = try render(grid.scrollView)
        XCTAssertGreaterThan(orangePixels(current, in: NSRect(x: 0, y: 2 * 22, width: 100, height: 22)), 20)
        XCTAssertEqual(accentPixels(current, in: NSRect(x: 0, y: 2 * 22, width: 3, height: 22)), 0)
        // Without highlights, the active cell has its ring.
        grid.highlighter = nil
        let plain = try render(grid.scrollView)
        XCTAssertGreaterThan(accentPixels(plain, in: NSRect(x: 0, y: 2 * 22, width: 3, height: 22)), 10)
        XCTAssertEqual(yellowPixels(plain, in: NSRect(x: 0, y: 2 * 22, width: 100, height: 22)), 0)
    }

    func testHighlightRangesFollowTheShownSymbols() {
        // A CRLF shows as one ↵, so later ranges move back by one.
        let value = "a\r\nb marlow"
        XCTAssertEqual(CellText.display(value).text, "a↵b marlow")
        XCTAssertEqual(CellText.displayRanges([NSRange(location: 5, length: 6)], in: value), [NSRange(location: 4, length: 6)])
        XCTAssertEqual(CellText.displayRanges([NSRange(location: 1, length: 2)], in: value), [NSRange(location: 1, length: 1)])
        // A lone LF, a tab and no CR: unchanged.
        XCTAssertEqual(CellText.displayRanges([NSRange(location: 2, length: 3)], in: "a\nb\tcd"), [NSRange(location: 2, length: 3)])
    }

    private func render(_ view: NSView) throws -> NSBitmapImageRep {
        view.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
        view.cacheDisplay(in: view.bounds, to: rep)
        return rep
    }

    private func pixels(_ rep: NSBitmapImageRep, in rect: NSRect, _ test: (NSColor) -> Bool) -> Int {
        let scale = CGFloat(rep.pixelsWide) / rep.size.width
        var count = 0
        for y in Int(rect.minY * scale)..<Int(rect.maxY * scale) {
            for x in Int(rect.minX * scale)..<Int(rect.maxX * scale) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), test(color) { count += 1 }
            }
        }
        return count
    }

    private func yellowPixels(_ rep: NSBitmapImageRep, in rect: NSRect) -> Int {
        pixels(rep, in: rect) { $0.redComponent > 0.85 && $0.greenComponent > 0.75 && $0.blueComponent < 0.75 }
    }

    private func orangePixels(_ rep: NSBitmapImageRep, in rect: NSRect) -> Int {
        pixels(rep, in: rect) { $0.redComponent > 0.85 && $0.greenComponent > 0.35 && $0.greenComponent < 0.75 && $0.blueComponent < 0.35 }
    }

    private func accentPixels(_ rep: NSBitmapImageRep, in rect: NSRect) -> Int {
        pixels(rep, in: rect) { $0.blueComponent > 0.6 && $0.redComponent < 0.4 }
    }

    // MARK: Words and keys

    func testTheFindCountReadsAsTheMockupDoes() {
        XCTAssertEqual(FindText.count(current: 6, total: 2_318, complete: true, searching: false), "6 of 2,318")
        XCTAssertEqual(FindText.count(current: 6, total: 2_318, complete: false, searching: true), "6 of 2,318+")
        XCTAssertEqual(FindText.count(current: nil, total: 2_318, complete: true, searching: false), "2,318 matches")
        XCTAssertEqual(FindText.count(current: nil, total: 1, complete: true, searching: false), "1 match")
        XCTAssertEqual(FindText.count(current: nil, total: 0, complete: true, searching: false), "No matches")
        XCTAssertEqual(FindText.count(current: nil, total: 0, complete: false, searching: true), "Searching…")
    }

    /// ⌘↩ belongs to the inspector (ADR-0002 question 11): its text view
    /// takes it, so it can't reach the grid's Insert Row (task 2.5a).
    func testCommandReturnIsTheInspectorsOwn() throws {
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 300), styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        addTeardownBlock { window.close() }
        let inspector = CellInspectorView(frame: window.contentView!.bounds)
        window.contentView = inspector
        var commits = 0
        inspector.textView.onCommit = { commits += 1 }
        window.makeFirstResponder(inspector.textView)
        let event = { (characters: String, code: UInt16, flags: NSEvent.ModifierFlags) in
            try XCTUnwrap(NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: window.windowNumber,
                context: nil, characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code
            ))
        }
        // These events go to the view directly, never to the system.
        XCTAssertTrue(inspector.textView.performKeyEquivalent(with: try event("\r", 36, .command)))
        XCTAssertTrue(inspector.textView.performKeyEquivalent(with: try event("\u{3}", 76, .command)))
        XCTAssertEqual(commits, 2)
        XCTAssertFalse(InspectorTextView.isCommit(try event("\r", 36, [])))
        XCTAssertFalse(InspectorTextView.isCommit(try event("\r", 36, [.command, .shift])))
    }

    /// The shortcuts of DESIGN §4.2 and mockup 06c that phase 1 has, in the
    /// menu bar.
    func testTheMenusHaveTheShortcuts() throws {
        let menu = MainMenu.make()
        func item(_ action: Selector) throws -> NSMenuItem {
            func find(_ menu: NSMenu) -> NSMenuItem? {
                for item in menu.items {
                    if item.action == action { return item }
                    if let submenu = item.submenu, let found = find(submenu) { return found }
                }
                return nil
            }
            return try XCTUnwrap(find(menu), "no item for \(action)")
        }
        let expected: [(Selector, String, NSEvent.ModifierFlags)] = [
            (#selector(DocumentViewController.showFind(_:)), "f", .command),
            (#selector(DocumentViewController.findNext(_:)), "g", .command),
            (#selector(DocumentViewController.findPrevious(_:)), "g", [.command, .shift]),
            (#selector(DocumentViewController.goToRow(_:)), "l", .command),
            (#selector(NSText.copy(_:)), "c", .command),
            (#selector(NSText.selectAll(_:)), "a", .command),
            (#selector(DocumentViewController.toggleCellInspector(_:)), "i", .command),
        ]
        for (action, key, modifiers) in expected {
            let found = try item(action)
            XCTAssertEqual(found.keyEquivalent, key, "\(action)")
            XCTAssertEqual(found.keyEquivalentModifierMask, modifiers, "\(action)")
        }
    }
}
