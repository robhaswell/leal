import AppKit

/// The sticky header row. It sits above the scroll view and follows its
/// horizontal offset, so it never scrolls away. Dragging a column's right
/// edge resizes the column, and double-clicking the edge fits it to its
/// contents.
@MainActor
final class GridHeaderView: NSView {
    weak var dataSource: (any GridDataSource)?
    var geometry = GridLayout() {
        didSet { placeEditor() }
    }
    /// The scroll view's horizontal offset.
    var offsetX: CGFloat = 0 {
        didSet {
            guard offsetX != oldValue else { return }
            needsDisplay = true
            placeEditor()
        }
    }
    /// The menu for a right-click (or Control-click) on column `column`'s
    /// title: "Rename Column…" (task 2.5.1).
    var menuForColumn: ((_ column: Int) -> NSMenu?)?
    /// The header row's editor (task 2.5.1), over its column's title, and
    /// the column. It follows the grid's sideways scroll and the column's
    /// width.
    private(set) var editor: (view: NSView, column: Int)?

    /// The user dragged column `column` to `width`.
    var onResize: ((_ column: Int, _ width: CGFloat) -> Void)?
    /// The user double-clicked column `column`'s edge.
    var onFit: ((_ column: Int) -> Void)?
    /// Scroll events over the header scroll the grid.
    weak var scrollTarget: NSView?

    private var titleLines: [Int: TextLine] = [:]
    /// How many times it has drawn, for the scroll benchmark.
    private(set) var draws = 0

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }
    override var isOpaque: Bool { true }

    func invalidateContent() {
        titleLines.removeAll()
        needsDisplay = true
    }

    /// Shows `view` over column `column`'s title, as its editor.
    func showEditor(_ view: NSView, column: Int) {
        editor = (view, column)
        if view.superview !== self { addSubview(view) }
        placeEditor()
    }

    /// Where the editor's column's title is now, in this view.
    func titleRect(column: Int) -> CGRect {
        let rect = geometry.cellRect(row: 0, column: column).offsetBy(dx: -offsetX, dy: 0)
        return CGRect(x: rect.minX, y: 0, width: rect.width, height: bounds.height - 1)
    }

    private func placeEditor() {
        guard let editor, editor.column < geometry.columnCount else { return }
        let frame = titleRect(column: editor.column)
        if editor.view.frame != frame { editor.view.frame = frame }
    }

    override func willRemoveSubview(_ subview: NSView) {
        super.willRemoveSubview(subview)
        if subview === editor?.view { editor = nil }
    }

    override func menu(for event: NSEvent) -> NSMenu? {
        let x = convert(event.locationInWindow, from: nil).x + offsetX
        guard let column = geometry.column(atX: x) else { return nil }
        return menuForColumn?(column)
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        invalidateContent()
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        draws += 1
        let palette = GridPalette.current()
        context.setFillColor(palette.headerBackground)
        context.fill(dirtyRect)
        if let source = dataSource {
            let columns = geometry.columnRange(minX: offsetX + dirtyRect.minX, maxX: offsetX + dirtyRect.maxX)
            for column in columns {
                let rect = geometry.cellRect(row: 0, column: column).offsetBy(dx: -offsetX, dy: 0)
                let cellRect = CGRect(x: rect.minX, y: 0, width: rect.width, height: bounds.height)
                let line = titleLines[column] ?? {
                    let title = source.headerTitle(column: column)
                    let (font, color) = style(title.style, palette: palette)
                    let line = CellPainter.makeLine(title.text, font: font, color: color, symbolColor: palette.secondaryText)
                    titleLines[column] = line
                    return line
                }()
                let font = style(source.headerTitle(column: column).style, palette: palette).font
                CellPainter.drawText(line, in: cellRect, font: font, alignment: .leading, context: context, ellipsisColor: palette.text)
                CellPainter.drawColumnSeparator(atX: cellRect.maxX, minY: 0, maxY: bounds.height, palette: palette, context: context)
            }
        }
        context.setFillColor(palette.gridLine)
        context.fill(CGRect(x: dirtyRect.minX, y: bounds.height - 1, width: dirtyRect.width, height: 1))
    }

    private func style(_ style: HeaderStyle, palette: GridPalette) -> (font: NSFont, color: CGColor) {
        switch style {
        case .name: (GridFonts.header, palette.text)
        case .number: (GridFonts.cell, palette.secondaryText)
        case .extra: (GridFonts.header, palette.tertiaryText)
        }
    }

    // MARK: Resizing

    /// The resize cursor over a column's edge. It is set as the pointer
    /// moves, from one tracking area, rather than with a cursor rectangle
    /// per edge: those would be rebuilt on every frame of a horizontal
    /// scroll.
    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        for area in trackingAreas { removeTrackingArea(area) }
        addTrackingArea(NSTrackingArea(
            rect: .zero,
            options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect, .cursorUpdate],
            owner: self
        ))
    }

    override func cursorUpdate(with event: NSEvent) {
        updateCursor(event)
    }

    override func mouseMoved(with event: NSEvent) {
        updateCursor(event)
    }

    override func mouseExited(with event: NSEvent) {
        NSCursor.arrow.set()
    }

    private func updateCursor(_ event: NSEvent) {
        let x = convert(event.locationInWindow, from: nil).x + offsetX
        (geometry.columnEdge(nearX: x) == nil ? NSCursor.arrow : NSCursor.resizeLeftRight).set()
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard let column = geometry.columnEdge(nearX: point.x + offsetX) else { return }
        if event.clickCount == 2 {
            onFit?(column)
            return
        }
        let startX = point.x
        let startWidth = geometry.widths[column]
        // Track the drag until the button comes up. Each move resizes the
        // column live.
        while let next = window?.nextEvent(matching: [.leftMouseDragged, .leftMouseUp]) {
            let x = convert(next.locationInWindow, from: nil).x
            onResize?(column, max(GridMetrics.resizeMinimumWidth, startWidth + x - startX))
            if next.type == .leftMouseUp { break }
        }
    }

    override func scrollWheel(with event: NSEvent) {
        scrollTarget?.scrollWheel(with: event)
    }
}

/// The row-number gutter. It is as tall as the grid and sits in its own
/// clip view left of the scroll view, which follows the grid's vertical
/// scrolling; like the grid, it is drawn into strips that the scroll moves
/// (`GridStrips`).
/// The active cell's row number is in the accent colour (ADR-0002
/// question 3); rows not read yet have no number (question 4).
@MainActor
final class GridGutterView: StripContentView {
    weak var dataSource: (any GridDataSource)?
    var rowHeight = GridMetrics.rowHeight
    var activeRow: Int? {
        didSet {
            guard activeRow != oldValue else { return }
            for row in [oldValue, activeRow].compactMap({ $0 }) {
                invalidate(NSRect(x: 0, y: CGFloat(row) * rowHeight, width: bounds.width, height: rowHeight))
            }
        }
    }
    /// The selected rows, when the selection spans more than one (task
    /// 1.8): their numbers are in the accent colour too.
    var selectedRows: ClosedRange<Int>? {
        didSet {
            guard selectedRows != oldValue else { return }
            for rows in [oldValue, selectedRows].compactMap({ $0 }) {
                let rect = NSRect(x: 0, y: CGFloat(rows.lowerBound) * rowHeight, width: bounds.width, height: CGFloat(rows.count) * rowHeight)
                invalidate(rect)
            }
        }
    }
    /// The top of the visible part: the grid's vertical offset.
    var offsetY: CGFloat { visibleRect.minY }

    weak var scrollTarget: NSView?
    /// A click on a row number: the row is selected.
    var onClick: ((_ row: Int) -> Void)?
    /// A Shift-click on a row number: the selected rows run to it.
    var onExtend: ((_ row: Int) -> Void)?

    /// Row numbers, by row: unlike cells' values each is drawn once in a
    /// while, so they are kept like the cells' lines (task 2.0a: emptying a
    /// dictionary of 2,000 lines at once cost a frame).
    private let numbers = LineCache<Int>(capacity: 1_000)
    /// Lays out the numbers of rows about to scroll into view (task 2.0a).
    private let ahead = NumberReadAhead()
    /// How many numbers it has asked for ahead, for tests.
    var numbersAskedAhead: Int { ahead.numbersAsked }
    /// The numbers' text, drawn together (task 2.0a).
    private let glyphs = GlyphBatch()
    /// Selected rows' numbers, in the accent colour.
    private var selectedNumbers: [Int: TextLine] = [:]
    /// How many times it has drawn, for the scroll benchmark.
    private(set) var draws = 0
    private var activeNumber: (row: Int, line: TextLine)?

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
    override var stripRowHeight: CGFloat { rowHeight }

    /// The width that fits the numbers of `rows` rows.
    static func width(rows: Int) -> CGFloat {
        let digits = String(max(1, rows)).count
        let digit = TextMeasurer(font: GridFonts.gutter).width(of: "0")
        return max(GridMetrics.gutterMinimumWidth, (CGFloat(digits) * digit + 2 * GridMetrics.cellPadding + 8).rounded(.up))
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        numbers.removeAll()
        ahead.reset()
        selectedNumbers.removeAll()
        activeNumber = nil
        needsDisplay = true
    }

    /// Draws the numbers in `dirtyRect`: for AppKit's drawing, or for a
    /// strip (`GridStrips`).
    override func drawContent(in dirtyRect: CGRect, context: CGContext) {
        draws += 1
        let palette = GridPalette.current()
        context.setFillColor(palette.gutterBackground)
        context.fill(dirtyRect)
        context.setFillColor(palette.gridLine)
        context.fill(CGRect(x: bounds.maxX - 1, y: dirtyRect.minY, width: 1, height: dirtyRect.height))
        guard let source = dataSource else { return }
        let loaded = min(source.loadedRowCount, source.rowCount)
        let first = max(0, Int((dirtyRect.minY / rowHeight).rounded(.down)))
        let end = min(loaded, Int((dirtyRect.maxY / rowHeight).rounded(.up)))
        guard first < end else { return }
        for row in first..<end {
            let rect = CGRect(x: 0, y: CGFloat(row) * rowHeight, width: bounds.width - 4, height: rowHeight)
            let isActive = row == activeRow
            let font = isActive ? GridFonts.gutterActive : GridFonts.gutter
            let line: TextLine
            if isActive {
                if let cached = activeNumber, cached.row == row {
                    line = cached.line
                } else {
                    line = CellPainter.makeLine(String(row + 1), font: font, color: palette.accent, symbolColor: palette.accent)
                    activeNumber = (row, line)
                }
            } else if selectedRows?.contains(row) == true {
                line = selectedNumbers[row] ?? {
                    if selectedNumbers.count > 2_000 { selectedNumbers.removeAll() }
                    let made = CellPainter.makeLine(String(row + 1), font: font, color: palette.accent, symbolColor: palette.accent)
                    selectedNumbers[row] = made
                    return made
                }()
            } else {
                line = numbers.line(for: row) {
                    CellPainter.makeLine(String(row + 1), font: font, color: palette.secondaryText, symbolColor: palette.secondaryText)
                }
            }
            CellPainter.addText(line, in: rect, font: font, alignment: .trailing, to: glyphs, context: context, ellipsisColor: palette.secondaryText)
            if source.rowHasMarker(row) {
                glyphs.draw(in: context)
                CellPainter.drawGutterMarker(rowRect: rect, context: context)
            }
        }
        glyphs.draw(in: context)
        ahead.update(
            visible: visibleRect,
            rowHeight: rowHeight,
            loaded: loaded,
            font: GridFonts.gutter,
            color: palette.secondaryText,
            numbers: numbers
        )
    }

    override func mouseDown(with event: NSEvent) {
        let y = convert(event.locationInWindow, from: nil).y
        guard y >= 0, let source = dataSource else { return }
        let row = Int((y / rowHeight).rounded(.down))
        guard row < source.rowCount else { return }
        if event.modifierFlags.contains(.shift), let onExtend {
            onExtend(row)
        } else {
            onClick?(row)
        }
    }

    override func scrollWheel(with event: NSEvent) {
        scrollTarget?.scrollWheel(with: event)
    }
}

/// The box above the gutter, left of the header.
final class GridCornerView: NSView {
    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        guard let context = NSGraphicsContext.current?.cgContext else { return }
        let palette = GridPalette.current()
        context.setFillColor(palette.headerBackground)
        context.fill(bounds)
        context.setFillColor(palette.gridLine)
        context.fill(CGRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1))
        context.fill(CGRect(x: bounds.width - 1, y: 0, width: 1, height: bounds.height))
    }
}

/// The grid's scroll view. Its scroll events tell the core's scheduler the
/// user is interacting, so background work pauses while they scroll
/// (DESIGN §3.10 rule 3): every event is input, and a gesture, including
/// its momentum, is one interaction from start to end.
@MainActor
final class GridScrollView: NSScrollView {
    /// The clip view scrolled: header and gutter follow.
    var onScroll: (() -> Void)?
    /// A scroll event arrived.
    var onScrollInput: (() -> Void)?
    /// A scroll gesture began (`true`) or ended, momentum included (`false`).
    var onGesture: ((Bool) -> Void)?

    /// Whether a scroll gesture (or its momentum) is under way.
    private(set) var isInGesture = false

    override func scrollWheel(with event: NSEvent) {
        onScrollInput?()
        if event.phase.contains(.began) || event.momentumPhase.contains(.began) {
            setGesture(true)
        }
        super.scrollWheel(with: event)
        if event.momentumPhase.contains(.ended) || event.momentumPhase.contains(.cancelled)
            || event.phase.contains(.cancelled)
            || (event.phase.contains(.ended) && event.momentumPhase.isEmpty)
        {
            // A gesture that ends without momentum ends here; one with
            // momentum gets `.began` again from its momentum phase.
            setGesture(false)
        }
    }

    /// Reports a gesture beginning or ending, once each.
    func setGesture(_ active: Bool) {
        guard active != isInGesture else { return }
        isInGesture = active
        onGesture?(active)
    }

    /// A window closed mid-gesture never sends the gesture's end, and the
    /// scheduler has no timeout: end it here, or background work would stay
    /// paused for every document.
    override func viewWillMove(toWindow newWindow: NSWindow?) {
        if newWindow == nil {
            setGesture(false)
        }
        super.viewWillMove(toWindow: newWindow)
    }

    override func reflectScrolledClipView(_ clipView: NSClipView) {
        super.reflectScrolledClipView(clipView)
        onScroll?()
    }

    /// The clip view and scrollers were laid out: the strips' view covers
    /// the clip view again.
    var onTile: (() -> Void)?

    override func tile() {
        super.tile()
        onTile?()
    }
}
