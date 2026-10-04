import AppKit
import QuartzCore

/// A cell's position in the grid.
struct CellPosition: Equatable, Hashable, Sendable {
    var row: Int
    var column: Int
}

/// How many rows above and below its own a row's text's ink reaches
/// (stacked marks), for the strips (task 2.0b).
struct InkSpill: Equatable, Sendable {
    var above = 0
    var below = 0

    func union(_ other: InkSpill) -> InkSpill {
        InkSpill(above: max(above, other.above), below: max(below, other.below))
    }
}

/// The grid's cells: the document view of the grid's scroll view (ADR-0001
/// option B). It is as tall as all the rows, but draws only the cells in
/// the area AppKit asks for, with Core Text, from `TextLineCache`.
///
/// It is the grid's first responder: keys become `GridMove`s and clicks
/// select cells, both handed to `GridContainerView` through closures.
@MainActor
final class GridView: StripContentView, NSMenuItemValidation {
    weak var dataSource: (any GridDataSource)?
    var geometry = GridLayout()
    /// The selection (task 1.8): the active cell (ADR-0002 question 3) and
    /// the rectangle of selected cells.
    var selection: GridSelection? {
        didSet {
            guard selection != oldValue else { return }
            // (Not cut to the visible rectangle: strips ahead of the scroll
            // show the selection too.)
            for rect in [oldValue, selection].compactMap({ $0.flatMap(selectionRect) }) {
                invalidate(rect.insetBy(dx: -2, dy: -2))
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
    /// Return or a double-click: edit the active cell (task 2.5.1).
    var onEdit: (() -> Void)?
    /// A key that types text: edit the active cell, starting from that
    /// text (DESIGN §4.2, "start typing"). The event goes on to the editor,
    /// so input methods compose from it.
    var onTypeToEdit: ((NSEvent) -> Void)?
    /// ⌘↩, ⇧⌘↩ or ⌘⌫ reached the grid (task 2.5a): Edit > Insert Row
    /// Below, Duplicate Row or Delete Row is off, or it would have taken
    /// the key first.
    var onRowCommandKey: ((RowCommandKey) -> Void)?

    let lines = TextLineCache(capacity: 2_500)
    /// Lays out the text of rows about to scroll into view (task 2.0a).
    let ahead = LineReadAhead()
    /// The cells' text, drawn together (task 2.0a).
    private let glyphs = GlyphBatch()
    /// Rectangles filled together, kept between draws.
    private var rects: [CGRect] = []
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
    /// Opaque when it draws itself; with strips its layer is empty.
    override var isOpaque: Bool { strips == nil }
    override var acceptsFirstResponder: Bool { true }

    /// The cached lines carry colours, so a new appearance needs new ones.
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        lines.removeAll()
        ahead.reset()
        forgetSpills()
        needsDisplay = true
    }

    /// Forget every laid-out line: the values or the column styles changed.
    func invalidateContent() {
        lines.removeAll()
        ahead.reset()
        forgetSpills()
        needsDisplay = true
    }

    override var stripRowHeight: CGFloat { geometry.rowHeight }

    /// Draws the cells in `dirtyRect`, in this view's coordinates, into
    /// `context`: for AppKit's drawing (`draw(_:)`), or for a strip
    /// (`GridStrips`), the same drawing either way.
    override func drawContent(in dirtyRect: CGRect, context: CGContext) {
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
        CellPainter.drawRowBackgrounds(
            rows: shaded,
            minX: dirtyRect.minX,
            width: dirtyRect.width,
            rowHeight: geometry.rowHeight,
            palette: palette,
            context: context,
            scratch: &rects
        )
        guard let source else { return }
        let rows = geometry.rowRange(minY: dirtyRect.minY, maxY: dirtyRect.maxY, rows: rowCount)
        let columns = geometry.columnRange(minX: dirtyRect.minX, maxX: dirtyRect.maxX)
        if !rows.isEmpty, !columns.isEmpty {
            // With strips, rows outside `rows` whose ink spills into them
            // are drawn too (cut to the strip): see `spills`.
            let drawn = strips == nil ? [rows] : rowsReaching(rows, rowCount: rowCount)
            let span = drawn[0].lowerBound..<drawn[drawn.count - 1].upperBound
            source.prepare(rows: span, columns: columns)
            highlighter?.prepareHighlights(rows: span, columns: columns)
            drawCells(
                rows: drawn,
                own: rows,
                columns: columns,
                loaded: loaded,
                source: source,
                palette: palette,
                context: context
            )
            if firstDrawTime == nil, loaded > 0 {
                firstDrawTime = CACurrentMediaTime()
                onFirstRows?()
                onFirstRows = nil
            }
        }
        CellPainter.drawColumnSeparators(
            atX: columns.lazy.map { self.geometry.offsets[$0 + 1] },
            minY: dirtyRect.minY,
            maxY: dirtyRect.maxY,
            palette: palette,
            context: context,
            scratch: &rects
        )
        if let active = activeCell, active.row < rowCount, active.column < geometry.columnCount,
           !isCurrentMatchShown(active)
        {
            let rect = geometry.cellRect(row: active.row, column: active.column)
            if rect.intersects(dirtyRect) {
                CellPainter.drawActiveCellRing(in: rect, palette: palette, context: context)
            }
        }
        // With strips, the rows ahead are drawn in all their columns.
        let drawnWidth = strips.map { (minX: CGFloat(0), maxX: $0.stripWidth) }
        ahead.update(visible: visibleRect, drawnWidth: drawnWidth, geometry: geometry, source: source, palette: palette, lines: lines, caretOffsets: highlighter != nil)
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

    // MARK: Ink that spills past its row (task 2.0b)

    /// Rows whose text's ink spills past them (stacked marks: "Z̵̧̢̛"), with
    /// how many rows it reaches above and below. A strip ends between two
    /// rows, and Core Animation cuts its drawing there; so a strip also
    /// draws the rows outside it whose ink reaches it (cut to the strip),
    /// and what is shown is what drawing the whole view at once shows. It
    /// is learnt as rows are drawn into strips; a row first drawn after a
    /// strip its ink reaches redraws that part of the strip (the next
    /// frame). Without strips, AppKit's drawing is as it was.
    private var spills: [Int: InkSpill] = [:]
    /// More spills than this are forgotten, and everything is drawn again
    /// (lowered in tests).
    var spillRecordLimit = 10_000
    /// The most rows any ink in `spills` reaches: how far from a strip to
    /// look.
    private var farthestSpill = 0

    /// `rows` and the rows their ink spilled into when last drawn: an
    /// edit's rows are redrawn together with those, in the same pass, so a
    /// neighbour that held ink of the old value is cleared at once (a
    /// row's new ink is learnt when it is drawn: `noteSpill`).
    func rows(withSpillsOf rows: Range<Int>) -> Range<Int> {
        guard !spills.isEmpty, !rows.isEmpty else { return rows }
        var low = rows.lowerBound
        var high = rows.upperBound
        let reaching: [(Int, InkSpill)] = rows.count <= spills.count
            ? rows.compactMap { row in spills[row].map { (row, $0) } }
            : spills.filter { rows.contains($0.key) }.map { ($0.key, $0.value) }
        for (row, spill) in reaching {
            low = min(low, max(0, row - spill.above))
            high = max(high, row + spill.below + 1)
        }
        return low..<high
    }

    /// What was learnt is about values and fonts that may be different now.
    private func forgetSpills() {
        spills.removeAll()
        farthestSpill = 0
    }

    /// `rows`, and the rows outside them whose ink spills into them, in
    /// order: each a range.
    private func rowsReaching(_ rows: Range<Int>, rowCount: Int) -> [Range<Int>] {
        guard farthestSpill > 0 else { return [rows] }
        var drawn: [Range<Int>] = []
        for row in max(0, rows.lowerBound - farthestSpill)..<rows.lowerBound {
            if let spill = spills[row], row + spill.below >= rows.lowerBound { drawn.append(row..<(row + 1)) }
        }
        drawn.append(rows)
        for row in rows.upperBound..<min(rowCount, rows.upperBound + farthestSpill) {
            if let spill = spills[row], row - spill.above < rows.upperBound { drawn.append(row..<(row + 1)) }
        }
        return drawn
    }

    /// Row `row` was drawn, in all its columns (`whole`) or some, and its
    /// ink reaches `spill`: if that is news, the rows outside `own` (those
    /// being drawn) that its ink reaches now, or reached before, are drawn
    /// again.
    private func noteSpill(_ spill: InkSpill, row: Int, whole: Bool, own: Range<Int>) {
        let old = spills[row] ?? InkSpill()
        let new = whole ? spill : old.union(spill)
        guard new != old else { return }
        if spills.count > spillRecordLimit {
            // Rows already drawn lose their record: draw everything again,
            // which learns the spills that matter in view.
            forgetSpills()
            strips?.invalidateAll()
        }
        if new == InkSpill() {
            spills[row] = nil
        } else {
            spills[row] = new
            farthestSpill = max(farthestSpill, new.above, new.below)
        }
        // The rows the ink reaches, now or before, less those being drawn.
        let reach = old.union(new)
        let height = geometry.rowHeight
        let low = max(0, row - reach.above)
        let high = row + reach.below + 1
        for (first, end) in [(low, min(high, own.lowerBound)), (max(low, own.upperBound), high)] where first < end {
            invalidate(CGRect(x: 0, y: CGFloat(first) * height, width: bounds.width, height: CGFloat(end - first) * height))
        }
    }

    /// Draws the cells of `rows` (in order) in `columns`. `own` are the
    /// rows of the area being drawn; the others are only there for their
    /// ink that spills into it.
    private func drawCells(
        rows: [Range<Int>],
        own: Range<Int>,
        columns: Range<Int>,
        loaded: Int,
        source: any GridDataSource,
        palette: GridPalette,
        context: CGContext
    ) {
        let numeric = columns.map { source.isNumeric(column: $0) }
        let learning = strips != nil
        let whole = columns == 0..<geometry.columnCount
        for row in rows.joined() {
            var spill = InkSpill()
            defer {
                if learning { noteSpill(spill, row: row, whole: whole, own: own) }
            }
            for column in columns {
                let rect = geometry.cellRect(row: row, column: column)
                let alignment: CellAlignment = numeric[column - columns.lowerBound] ? .trailing : .leading
                let highlight = highlighter?.highlight(row: row, column: column)
                if selection?.contains(row: row, column: column) == true,
                   !(highlight?.isCurrent == true && highlight?.ranges.isEmpty == false)
                {
                    glyphs.draw(in: context)
                    CellPainter.drawActiveCellFill(in: rect, palette: palette, context: context)
                }
                guard row < loaded else {
                    glyphs.draw(in: context)
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                    continue
                }
                let cell = source.cell(row: row, column: column)
                if source.isEdited(row: row, column: column) {
                    glyphs.draw(in: context)
                    let rightToLeft = if case let .text(value, _) = cell { CellPainter.isRightToLeft(value) } else { false }
                    CellPainter.drawEditedMark(in: rect, rightToLeft: rightToLeft, palette: palette, context: context)
                }
                switch cell {
                case let .text(value, truncated):
                    guard !value.isEmpty || truncated else { continue }
                    let number = alignment == .trailing
                    let font = number ? GridFonts.number : GridFonts.cell
                    let request = LineRequest(value: value, truncated: truncated, width: rect.width, number: number)
                    let line = lines.line(for: request.key) {
                        CellPainter.makeCellLine(request.shown, truncated: request.ellipsis, font: font, palette: palette)
                    }
                    if let highlight, !highlight.ranges.isEmpty {
                        glyphs.draw(in: context)
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
                    CellPainter.addText(line, in: rect, font: font, alignment: alignment, to: glyphs, context: context, ellipsisColor: palette.text)
                    if learning {
                        spill = spill.union(CellPainter.inkSpill(of: line, in: rect, font: font))
                    }
                    cellsDrawn += 1
                case .notLoaded:
                    glyphs.draw(in: context)
                    CellPainter.drawSkeleton(row: row, column: column, in: rect, alignment: alignment, palette: palette, context: context)
                case .missing:
                    // A short ragged row's missing cells (ADR-0002 question 5).
                    if source.isHatched(row: row, column: column) {
                        glyphs.draw(in: context)
                        CellPainter.drawHatch(in: rect, palette: palette, context: context)
                    }
                }
            }
        }
        glyphs.draw(in: context)
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
            if event.clickCount == 2 { onEdit?() }
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
        if let onTypeToEdit, Self.typesText(event) {
            onTypeToEdit(event)
            return
        }
        if let key = Self.rowCommandKey(event) {
            // Not Return's edit, nor a beep with no reason given. A held
            // ⌘⌫ deletes once, not a row each repeat.
            if key == .delete, event.isARepeat { return }
            onRowCommandKey?(key)
            return
        }
        interpretKeyEvents([event])
    }

    /// Which row command's key `event` is (DESIGN §4.2): ⌘↩ (Return or
    /// Enter), ⇧⌘↩, or ⌘⌫, with no other modifier (Caps Lock on or off).
    /// `nil` for any other key.
    static func rowCommandKey(_ event: NSEvent) -> RowCommandKey? {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask).subtracting([.numericPad, .function, .capsLock])
        guard event.type == .keyDown else { return nil }
        switch (event.keyCode, modifiers) {
        case (36, .command), (76, .command): return .insertBelow
        case (36, [.command, .shift]), (76, [.command, .shift]): return .duplicate
        case (51, .command): return .delete
        default: return nil
        }
    }

    /// Whether `event` types text, rather than being a command: it gives
    /// characters, none a control character or a function key's (arrows,
    /// Page Up and the like are in the private use area), with no ⌘ or ⌃.
    /// A dead key (⌥E) types text too, though it gives no characters yet:
    /// the editor opens on it, so its input context composes the accent
    /// with the next key (task 2.5.1). An input method's keys give
    /// characters, and compose there too.
    static func typesText(_ event: NSEvent) -> Bool {
        guard event.type == .keyDown else { return false }
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        guard modifiers.isDisjoint(with: [.command, .control]), var characters = event.characters else { return false }
        if characters.isEmpty {
            // A dead key's characters are empty; its key's aren't.
            characters = event.charactersIgnoringModifiers ?? ""
            guard !characters.isEmpty else { return false }
        }
        return characters.unicodeScalars.allSatisfy { scalar in
            !CharacterSet.controlCharacters.contains(scalar) && !(0xF700...0xF8FF).contains(scalar.value)
        }
    }

    /// Return (and Enter): edit the active cell.
    override func insertNewline(_ sender: Any?) {
        onEdit?()
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

    /// Text that reaches the grid without `onTypeToEdit` (none in the
    /// app) is dropped: typing edits through the in-cell editor.
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

/// A row command's key (`GridView.rowCommandKey`, task 2.5a).
enum RowCommandKey: Equatable {
    /// ⌘↩: Insert Row Below.
    case insertBelow
    /// ⇧⌘↩: Duplicate Row.
    case duplicate
    /// ⌘⌫: Delete Row.
    case delete
}
