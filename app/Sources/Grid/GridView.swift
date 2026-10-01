import AppKit
import QuartzCore

/// A cell's position in the grid.
struct CellPosition: Equatable, Hashable, Sendable {
    var row: Int
    var column: Int
}

/// The grid's cells: the document view of the grid's scroll view (ADR-0001
/// option B). It is as tall as all the rows, but draws only the cells in
/// the area AppKit asks for, with Core Text, from `TextLineCache`.
///
/// It is the grid's first responder: keys become `GridMove`s and clicks
/// select cells, both handed to `GridContainerView` through closures.
@MainActor
final class GridView: NSView, NSMenuItemValidation {
    weak var dataSource: (any GridDataSource)?
    var geometry = GridLayout()
    /// The selection (task 1.8): the active cell (ADR-0002 question 3) and
    /// the rectangle of selected cells.
    var selection: GridSelection? {
        didSet {
            guard selection != oldValue else { return }
            let visible = visibleRect.insetBy(dx: -2, dy: -2)
            for rect in [oldValue, selection].compactMap({ $0.flatMap(selectionRect) }) {
                setNeedsDisplay(rect.insetBy(dx: -2, dy: -2).intersection(visible))
            }
        }
    }

    /// The active cell: one cell selected, or the one with the ring.
    var activeCell: CellPosition? {
        get { selection?.active }
        set { selection = newValue.map(GridSelection.init) }
    }

    /// Find's highlights (task 1.8), while the find bar is showing.
    weak var highlighter: (any GridHighlighter)? {
        didSet { needsDisplay = true }
    }

    /// A key asked to move the active cell (`extend`: with Shift, the
    /// selection's moving corner).
    var onMove: ((_ move: GridMove, _ extend: Bool) -> Void)?
    /// A click on a cell.
    var onClick: ((CellPosition) -> Void)?
    /// A Shift-click on a cell, or a drag over it from the clicked one.
    var onExtend: ((CellPosition) -> Void)?
    /// Edit > Select All (⌘A).
    var onSelectAll: (() -> Void)?
    /// Edit > Copy (⌘C).
    var onCopy: (() -> Void)?
    /// Whether there is something to copy, for the menu item.
    var canCopy: () -> Bool = { false }
    /// Any key or click: the user is interacting (DESIGN §3.10 rule 3).
    var onUserInput: (() -> Void)?

    let lines = TextLineCache()
    /// Cells drawn so far, for the scroll benchmark.
    private(set) var cellsDrawn = 0
    /// How many times, and how many points, it has drawn.
    private(set) var draws = 0
    private(set) var drawnArea: CGFloat = 0
    /// When the view first drew rows (`CACurrentMediaTime`), for the open
    /// to first rows budget (DESIGN §1).
    private(set) var firstDrawTime: CFTimeInterval?
    /// Called once, at that first draw with rows.
    var onFirstRows: (() -> Void)?

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }
    override var isOpaque: Bool { true }
    override var acceptsFirstResponder: Bool { true }

    /// The cached lines carry colours, so a new appearance needs new ones.
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        lines.removeAll()
        needsDisplay = true
    }

    /// Forget every laid-out line: the values or the column styles changed.
    func invalidateContent() {
        lines.removeAll()
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        draws += 1
        drawnArea += dirtyRect.width * dirtyRect.height
        let palette = GridPalette.current()
        let source = dataSource
        let rowCount = source?.rowCount ?? 0
        let loaded = min(source?.loadedRowCount ?? 0, rowCount)
        // Shade every row in the dirty area, including the empty ones below
        // the last row and right of the last column, as a table does.
        let shaded = geometry.rowRange(
            minY: dirtyRect.minY,
            maxY: dirtyRect.maxY,
            rows: Int((bounds.height / geometry.rowHeight).rounded(.up))
        )
        for row in shaded {
            let y = CGFloat(row) * geometry.rowHeight
            CellPainter.drawRowBackground(
                row: row,
                in: CGRect(x: dirtyRect.minX, y: y, width: dirtyRect.width, height: geometry.rowHeight),
                palette: palette,
                context: context
            )
        }
        guard let source else { return }
        let rows = geometry.rowRange(minY: dirtyRect.minY, maxY: dirtyRect.maxY, rows: rowCount)
        let columns = geometry.columnRange(minX: dirtyRect.minX, maxX: dirtyRect.maxX)
        if !rows.isEmpty, !columns.isEmpty {
            source.prepare(rows: rows, columns: columns)
            highlighter?.prepareHighlights(rows: rows, columns: columns)
            drawCells(rows: rows, columns: columns, loaded: loaded, source: source, palette: palette, context: context)
            if firstDrawTime == nil, loaded > 0 {
                firstDrawTime = CACurrentMediaTime()
                onFirstRows?()
                onFirstRows = nil
            }
        }
        for column in columns {
            CellPainter.drawColumnSeparator(
                atX: geometry.offsets[column + 1],
                minY: dirtyRect.minY,
                maxY: dirtyRect.maxY,
                palette: palette,
                context: context
            )
        }
        if let active = activeCell, active.row < rowCount, active.column < geometry.columnCount,
           !isCurrentMatchShown(active)
        {
            let rect = geometry.cellRect(row: active.row, column: active.column)
            if rect.intersects(dirtyRect) {
                CellPainter.drawActiveCellRing(in: rect, palette: palette, context: context)
            }
        }
    }

    /// The selection's cells, as one rectangle, if the grid has them.
    private func selectionRect(_ selection: GridSelection) -> CGRect? {
        let columns = geometry.columnCount
        guard columns > 0 else { return nil }
        let first = min(selection.columns.lowerBound, columns - 1)
        let last = min(selection.columns.upperBound, columns - 1)
        let top = geometry.cellRect(row: selection.rows.lowerBound, column: first)
        let bottom = geometry.cellRect(row: selection.rows.upperBound, column: last)
        return top.union(bottom)
    }

    /// The cell is find's current match and its highlight shows: mockup
    /// 04a draws that in place of the active cell's fill and ring.
    private func isCurrentMatchShown(_ cell: CellPosition) -> Bool {
        guard let highlight = highlighter?.highlight(row: cell.row, column: cell.column) else { return false }
        return highlight.isCurrent && !highlight.ranges.isEmpty
    }

    private func drawCells(
        rows: Range<Int>,
        columns: Range<Int>,
        loaded: Int,
        source: any GridDataSource,
        palette: GridPalette,
        context: CGContext
    ) {
        let numeric = columns.map { source.isNumeric(column: $0) }
        for row in rows {
            for column in columns {
                let rect = geometry.cellRect(row: row, column: column)
                let alignment: CellAlignment = numeric[column - columns.lowerBound] ? .trailing : .leading
                let highlight = highlighter?.highlight(row: row, column: column)
                if selection?.contains(row: row, column: column) == true,
                   !(highlight?.isCurrent == true && highlight?.ranges.isEmpty == false)
                {
                    CellPainter.drawActiveCellFill(in: rect, palette: palette, context: context)
                }
                guard row < loaded else {
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                    continue
                }
                switch source.cell(row: row, column: column) {
                case let .text(value, truncated):
                    guard !value.isEmpty || truncated else { continue }
                    let number = alignment == .trailing
                    let font = number ? GridFonts.number : GridFonts.cell
                    // Only what the column could show is laid out.
                    let fits = charactersThatFit(width: rect.width)
                    let cut = value.utf8.count > fits && value.count > fits
                    let shown = cut ? String(value.prefix(fits)) : value
                    let key = TextLineCache.Key(text: shown, truncated: truncated || cut, number: number)
                    let line = lines.line(for: key) {
                        CellPainter.makeCellLine(shown, truncated: truncated && !cut, font: font, palette: palette)
                    }
                    if let highlight, !highlight.ranges.isEmpty {
                        CellPainter.drawFindHighlights(
                            line,
                            ranges: CellText.displayRanges(highlight.ranges, in: shown),
                            in: rect,
                            alignment: alignment,
                            current: highlight.isCurrent,
                            context: context
                        )
                    }
                    CellPainter.drawText(line, in: rect, font: font, alignment: alignment, context: context, ellipsisColor: palette.text)
                    cellsDrawn += 1
                case .notLoaded:
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                case .missing:
                    // A short ragged row's missing cells (ADR-0002 question 5).
                    if source.isHatched(row: row, column: column) {
                        CellPainter.drawHatch(in: rect, context: context)
                    }
                }
            }
        }
    }

    // MARK: Input

    override func mouseDown(with event: NSEvent) {
        onUserInput?()
        window?.makeFirstResponder(self)
        guard let cell = cell(at: convert(event.locationInWindow, from: nil)) else { return }
        if event.modifierFlags.contains(.shift) {
            onExtend?(cell)
        } else {
            onClick?(cell)
        }
    }

    /// Dragging from the clicked cell selects the cells between (task
    /// 1.8), scrolling when the pointer leaves the visible area.
    override func mouseDragged(with event: NSEvent) {
        onUserInput?()
        autoscroll(with: event)
        guard let cell = cell(at: convert(event.locationInWindow, from: nil), clamped: true) else { return }
        onExtend?(cell)
    }

    /// The cell at `point`; with `clamped`, the nearest one when the point
    /// is outside the cells (a drag past the edge).
    func cell(at point: NSPoint, clamped: Bool = false) -> CellPosition? {
        let rows = dataSource?.rowCount ?? 0
        var point = point
        if clamped {
            guard rows > 0, geometry.columnCount > 0 else { return nil }
            point.y = min(max(0, point.y), geometry.height(rows: rows) - 1)
            point.x = min(max(0, point.x), geometry.totalWidth - 1)
        }
        guard
            let row = geometry.row(atY: point.y, rows: rows),
            let column = geometry.column(atX: point.x)
        else { return nil }
        return CellPosition(row: row, column: column)
    }

    override func keyDown(with event: NSEvent) {
        onUserInput?()
        interpretKeyEvents([event])
    }

    override func doCommand(by selector: Selector) {
        if let move = GridMove(selector: selector) {
            onMove?(move, false)
        } else if let move = GridMove(extendingSelector: selector) {
            onMove?(move, true)
        } else {
            super.doCommand(by: selector)
        }
    }

    // MARK: The Edit menu (task 1.8)

    // Neither Copy nor Select All is noted as user input: that would pause
    // P2 work for 250 ms (DESIGN §3.10 rule 3), and Copy's own job is P2.
    // Neither scrolls, so neither needs background work out of the way.

    @objc func copy(_ sender: Any?) {
        onCopy?()
    }

    override func selectAll(_ sender: Any?) {
        onSelectAll?()
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(copy(_:)): canCopy()
        case #selector(selectAll(_:)): (dataSource?.rowCount ?? 0) > 0 && geometry.columnCount > 0
        default: true
        }
    }

    /// Typing doesn't edit yet (phase 2).
    override func insertText(_ insertString: Any) {}
}

/// A keyboard move of the active cell (DESIGN §4.2), or with Shift of the
/// selection's moving corner (task 1.8).
enum GridMove: Equatable, Sendable {
    case up, down, left, right
    case pageUp, pageDown
    /// ⌘↑ and ⌘↓: the first and last row.
    case firstRow, lastRow
    /// ⌘← and ⌘→: the first and last column.
    case firstColumn, lastColumn
    /// Tab and Shift-Tab: the next and previous cell, along the row.
    case next, previous

    /// The move for one of AppKit's standard key-binding commands.
    init?(selector: Selector) {
        switch selector {
        case #selector(NSResponder.moveUp(_:)): self = .up
        case #selector(NSResponder.moveDown(_:)): self = .down
        case #selector(NSResponder.moveLeft(_:)): self = .left
        case #selector(NSResponder.moveRight(_:)): self = .right
        case #selector(NSResponder.pageUp(_:)), #selector(NSResponder.scrollPageUp(_:)): self = .pageUp
        case #selector(NSResponder.pageDown(_:)), #selector(NSResponder.scrollPageDown(_:)): self = .pageDown
        case #selector(NSResponder.moveToBeginningOfDocument(_:)),
             #selector(NSResponder.scrollToBeginningOfDocument(_:)):
            self = .firstRow
        case #selector(NSResponder.moveToEndOfDocument(_:)),
             #selector(NSResponder.scrollToEndOfDocument(_:)):
            self = .lastRow
        case #selector(NSResponder.moveToLeftEndOfLine(_:)),
             #selector(NSResponder.moveToBeginningOfLine(_:)):
            self = .firstColumn
        case #selector(NSResponder.moveToRightEndOfLine(_:)),
             #selector(NSResponder.moveToEndOfLine(_:)):
            self = .lastColumn
        case #selector(NSResponder.insertTab(_:)): self = .next
        case #selector(NSResponder.insertBacktab(_:)): self = .previous
        default: return nil
        }
    }

    /// The move for one of AppKit's Shift key-binding commands, which
    /// extend the selection.
    init?(extendingSelector selector: Selector) {
        switch selector {
        case #selector(NSResponder.moveUpAndModifySelection(_:)): self = .up
        case #selector(NSResponder.moveDownAndModifySelection(_:)): self = .down
        case #selector(NSResponder.moveLeftAndModifySelection(_:)): self = .left
        case #selector(NSResponder.moveRightAndModifySelection(_:)): self = .right
        case #selector(NSResponder.pageUpAndModifySelection(_:)): self = .pageUp
        case #selector(NSResponder.pageDownAndModifySelection(_:)): self = .pageDown
        case #selector(NSResponder.moveToBeginningOfDocumentAndModifySelection(_:)): self = .firstRow
        case #selector(NSResponder.moveToEndOfDocumentAndModifySelection(_:)): self = .lastRow
        case #selector(NSResponder.moveToLeftEndOfLineAndModifySelection(_:)),
             #selector(NSResponder.moveToBeginningOfLineAndModifySelection(_:)):
            self = .firstColumn
        case #selector(NSResponder.moveToRightEndOfLineAndModifySelection(_:)),
             #selector(NSResponder.moveToEndOfLineAndModifySelection(_:)):
            self = .lastColumn
        default: return nil
        }
    }

    /// Where the active cell goes from `cell`, in a grid of `rows` × `columns`
    /// that shows `pageRows` rows at once. Pure, so it is tested on its own.
    func apply(to cell: CellPosition, rows: Int, columns: Int, pageRows: Int) -> CellPosition {
        guard rows > 0, columns > 0 else { return cell }
        let page = max(1, pageRows - 1)
        var row = cell.row
        var column = cell.column
        switch self {
        case .up: row -= 1
        case .down: row += 1
        case .left: column -= 1
        case .right: column += 1
        case .pageUp: row -= page
        case .pageDown: row += page
        case .firstRow: row = 0
        case .lastRow: row = rows - 1
        case .firstColumn: column = 0
        case .lastColumn: column = columns - 1
        case .next:
            if column + 1 < columns {
                column += 1
            } else if row + 1 < rows {
                row += 1
                column = 0
            }
        case .previous:
            if column > 0 {
                column -= 1
            } else if row > 0 {
                row -= 1
                column = columns - 1
            }
        }
        return CellPosition(row: min(max(0, row), rows - 1), column: min(max(0, column), columns - 1))
    }
}
