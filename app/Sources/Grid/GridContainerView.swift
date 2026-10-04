import AppKit

/// The whole grid: the cells in a scroll view, with the sticky header
/// above, the row-number gutter on the left and the corner box between
/// them (ADR-0001 option B). It keeps them in step, owns the column
/// geometry and the active cell, and turns key presses into moves that keep
/// the active cell in view.
///
/// It knows nothing about files: everything it draws comes from its
/// `dataSource`.
@MainActor
final class GridContainerView: NSView {
    let scrollView = GridScrollView()
    let gridView = GridView()
    let headerView = GridHeaderView()
    let gutterView = GridGutterView()
    /// Holds the gutter, scrolled with the grid.
    let gutterClip = NSClipView()
    let cornerView = GridCornerView()
    let pill = IndexingPillView()
    /// Views over the cells that follow the scroll, above the strips: the
    /// in-cell editor's place (task 2.5).
    let overlay = GridOverlayView()

    /// The strips the cells are drawn into (ADR-0011), while the grid is
    /// narrow enough (`stripLine`); `nil` while AppKit draws them.
    private(set) var strips: GridStrips?
    /// Whether the drawing has been chosen once with the grid laid out:
    /// the first choice is made at the line itself, later ones with the
    /// hysteresis.
    private var drawingChosen = false
    /// The strips the gutter's numbers are drawn into. The gutter is never
    /// wide, so it always has them (unless `allowsStrips` is off).
    private(set) var gutterStrips: GridStrips?
    /// Whether the grid may draw into strips. Off only for the scroll
    /// benchmark's comparison with AppKit's drawing (`-LealStrips NO`, in
    /// bench builds) and for tests' reference drawing.
    let allowsStrips: Bool

    /// Grids wider than this many times the visible area keep AppKit's
    /// drawing (task 2.0b, refining ADR-0011's 4,096 pt line). A strip is
    /// as wide as the grid, so its drawing and its memory grow with the
    /// grid's width, AppKit's with the visible width; past about 1.5 times
    /// the visible width strips draw more a frame than AppKit does (the
    /// slowest frames, after the scroll jumps a screen, draw every strip in
    /// view), and add more memory (docs/tasks/2.0b.md, "Where strips pay").
    static let stripMaximumRatio: CGFloat = 1.5
    /// And never wider than this (ADR-0011's line): a very wide window
    /// would otherwise make very wide strips.
    static let stripMaximumWidth: CGFloat = 4_096
    /// A grid that went past the line goes back to strips only once it is
    /// this narrow (times the visible width, and in points), so a column
    /// dragged, or a window resized, back and forth across the line doesn't
    /// switch the drawing on every step.
    static let stripReturnRatio: CGFloat = 1.4
    static let stripReturnWidth: CGFloat = 3_840

    /// The widest the grid may be and draw into strips, with the visible
    /// area `visibleWidth` wide: the line, or with `returning` (it is drawn
    /// by AppKit now) the line less the hysteresis.
    static func stripLine(visibleWidth: CGFloat, returning: Bool) -> CGFloat {
        returning
            ? min(stripReturnWidth, stripReturnRatio * visibleWidth)
            : min(stripMaximumWidth, stripMaximumRatio * visibleWidth)
    }

    /// Strips unless a bench build was asked for AppKit's drawing.
    static var stripsByDefault: Bool {
        #if LEAL_BENCH
            UserDefaults.standard.object(forKey: "LealStrips") == nil || UserDefaults.standard.bool(forKey: "LealStrips")
        #else
            true
        #endif
    }

    weak var dataSource: (any GridDataSource)? {
        didSet {
            gridView.dataSource = dataSource
            headerView.dataSource = dataSource
            gutterView.dataSource = dataSource
            reloadData()
        }
    }

    /// Any key, click or scroll in the grid (DESIGN §3.10 rule 3).
    var onUserInput: (() -> Void)?
    /// A scroll gesture began or ended.
    var onGesture: ((Bool) -> Void)?
    /// The user resized a column by dragging its header edge.
    var onColumnResized: ((_ column: Int, _ width: CGFloat) -> Void)?
    /// The width that fits a column's contents, for a double-click on its
    /// header edge.
    var fittingWidth: ((_ column: Int) -> CGFloat?)?
    /// Whether every row is indexed: until then the last row is an
    /// estimate, and ⌘↓ waits for the real one (DESIGN §3.10 rule 5).
    var isIndexComplete: () -> Bool = { true }

    /// The selection changed (the active cell or the selected cells).
    var onSelectionChanged: (() -> Void)?
    /// Edit > Copy (⌘C) in the grid.
    var onCopy: (() -> Void)? {
        get { gridView.onCopy }
        set { gridView.onCopy = newValue }
    }
    /// Return or a double-click in the grid: edit the active cell (task
    /// 2.5.1).
    var onEdit: (() -> Void)? {
        get { gridView.onEdit }
        set { gridView.onEdit = newValue }
    }
    /// A key that types text in the grid: edit the active cell from it.
    var onTypeToEdit: ((NSEvent) -> Void)? {
        get { gridView.onTypeToEdit }
        set { gridView.onTypeToEdit = newValue }
    }
    /// The grid scrolled.
    var onScroll: (() -> Void)?

    private(set) var geometry = GridLayout()

    /// A move past the indexed rows that is waiting for the index to reach
    /// its target (DESIGN §3.10 rule 5): the grid shows the skeleton rows
    /// and the pill until then.
    enum PendingJump: Equatable {
        /// ⌘↓: the real last row, once indexing is complete.
        case end
        /// Go to Row (⌘L): this row, once it is indexed.
        case row(Int)
    }

    private(set) var pendingJump: PendingJump?
    /// The column a pending Go to Row selects in: the active cell's when
    /// it was asked for (no cell is selected while it waits).
    private var pendingColumn = 0

    /// The cell a pending ⌘↓ or Go to Row ended on, until the user moves
    /// again: if the visible area then shrinks (the find bar or a banner
    /// appearing just after), it is scrolled back into view, so the row
    /// jumped to is never left hidden (task 2.0 review).
    private var jumpedTo: CellPosition?
    /// When `jumpedTo` was set (`CACurrentMediaTime`): it is re-revealed
    /// only within `revealWindow` of that, so a shrink long after a jump
    /// leaves the view alone.
    private var jumpedAt: CFTimeInterval = 0
    /// See `jumpedAt`. Tests shorten it.
    static var revealWindow: CFTimeInterval = 1

    /// ⌘↓ went past the indexed rows: when the index is complete, the
    /// active cell moves to the real last row.
    var isJumpingToEnd: Bool { pendingJump == .end }

    /// The selection (task 1.8): the active cell and the selected cells.
    var selection: GridSelection? {
        get { gridView.selection }
        set {
            guard newValue != gridView.selection else { return }
            gridView.selection = newValue
            gutterView.activeRow = newValue?.active.row
            gutterView.selectedRows = newValue.flatMap { $0.rows.count > 1 ? $0.rows : nil }
            onSelectionChanged?()
        }
    }

    var activeCell: CellPosition? {
        get { selection?.active }
        set { selection = newValue.map(GridSelection.init) }
    }

    /// Find's highlights (task 1.8), while the find bar is showing.
    var highlighter: (any GridHighlighter)? {
        get { gridView.highlighter }
        set { gridView.highlighter = newValue }
    }

    override convenience init(frame: NSRect) {
        self.init(frame: frame, allowsStrips: Self.stripsByDefault)
    }

    init(frame: NSRect, allowsStrips: Bool) {
        self.allowsStrips = allowsStrips
        super.init(frame: frame)
        scrollView.documentView = gridView
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.automaticallyAdjustsContentInsets = false
        gutterClip.documentView = gutterView
        gutterClip.drawsBackground = false
        scrollView.onScroll = { [weak self] in self?.followScroll() }
        scrollView.onScrollInput = { [weak self] in self?.scrollInputArrived() }
        // A live scroll (a trackpad or a scroller drag) is scroll input too:
        // a jump's row is no longer to be kept in view.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollWillStart(_:)),
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        // The accent colour and Increase Contrast change the palette
        // (`GridPalette`); strips hold the old colours until redrawn.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(systemColorsChanged(_:)),
            name: NSColor.systemColorsDidChangeNotification,
            object: nil
        )
        NSWorkspace.shared.notificationCenter.addObserver(
            self,
            selector: #selector(systemColorsChanged(_:)),
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification,
            object: nil
        )
        scrollView.onGesture = { [weak self] began in self?.onGesture?(began) }
        headerView.scrollTarget = scrollView
        gutterView.scrollTarget = scrollView
        headerView.onResize = { [weak self] column, width in
            self?.setWidth(width, ofColumn: column)
            self?.onColumnResized?(column, width)
        }
        headerView.onFit = { [weak self] column in
            guard let self, let width = fittingWidth?(column) else { return }
            setWidth(width, ofColumn: column)
            onColumnResized?(column, width)
        }
        gridView.onMove = { [weak self] move, extend in self?.move(move, extend: extend) }
        gridView.onClick = { [weak self] cell in self?.select(cell) }
        gridView.onExtend = { [weak self] cell in self?.extend(to: cell) }
        gridView.onSelectAll = { [weak self] in self?.selectAll() }
        gridView.canCopy = { [weak self] in self?.selection != nil }
        gridView.onUserInput = { [weak self] in self?.onUserInput?() }
        gutterView.onClick = { [weak self] row in
            guard let self else { return }
            onUserInput?()
            window?.makeFirstResponder(gridView)
            selectRow(row)
        }
        gutterView.onExtend = { [weak self] row in
            guard let self else { return }
            onUserInput?()
            window?.makeFirstResponder(gridView)
            extendRows(to: row)
        }
        for view in [scrollView, headerView, gutterClip, cornerView, pill] as [NSView] {
            addSubview(view)
        }
        // Over the clip view, under the scrollers: the strips' view (when
        // there are strips), then the overlay.
        scrollView.addSubview(overlay, positioned: .above, relativeTo: scrollView.contentView)
        scrollView.onTile = { [weak self] in self?.placeStrips() }
        if allowsStrips {
            let gutterStrips = GridStrips(content: gutterView)
            addSubview(gutterStrips.view, positioned: .above, relativeTo: gutterClip)
            gutterView.strips = gutterStrips
            self.gutterStrips = gutterStrips
        }
        updateDrawing()
        pill.isHidden = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }
    /// Since the macOS 14 SDK views don't clip to their bounds by default,
    /// and `draw(_:)`'s rectangle may reach past them; this view's fills
    /// must not spill onto its neighbours.
    override var clipsToBounds: Bool {
        get { true }
        set {}
    }

    // MARK: Geometry

    /// Sets every column's width (from sizing).
    func setColumnWidths(_ widths: [CGFloat]) {
        geometry.setWidths(widths)
        applyLayout()
    }

    func setWidth(_ width: CGFloat, ofColumn column: Int) {
        geometry.setWidth(width, ofColumn: column)
        applyLayout()
    }

    private func applyLayout() {
        gridView.geometry = geometry
        headerView.geometry = geometry
        updateDocumentSize()
        gridView.needsDisplay = true
        headerView.needsDisplay = true
    }

    override func layout() {
        super.layout()
        // Whether the cell a jump ended on is in view before the visible
        // area changes size. The clip view's own rectangle: the view's
        // `visibleRect` is cut by this view's new size already.
        let jumpedCellWasShown = jumpedTo.map { scrollView.documentVisibleRect.contains(geometry.cellRect(row: $0.row, column: $0.column)) } ?? false
        let gutterWidth = GridGutterView.width(rows: dataSource?.rowCount ?? 0)
        let header = GridMetrics.headerHeight
        let width = bounds.width
        let height = bounds.height
        cornerView.frame = NSRect(x: 0, y: 0, width: gutterWidth, height: header)
        headerView.frame = NSRect(x: gutterWidth, y: 0, width: max(0, width - gutterWidth), height: header)
        gutterClip.frame = NSRect(x: 0, y: header, width: gutterWidth, height: max(0, height - header))
        scrollView.frame = NSRect(x: gutterWidth, y: header, width: max(0, width - gutterWidth), height: max(0, height - header))
        updateDocumentSize()
        placeStrips()
        positionPill()
        revealJumpedToCell(wasShown: jumpedCellWasShown)
    }

    /// A pending ⌘↓ or Go to Row ended on the active cell.
    private func noteJumpEnded() {
        jumpedTo = activeCell
        jumpedAt = CACurrentMediaTime()
    }

    /// The strips redraw with the new colours; AppKit's drawing, and the
    /// header, redraw as the views are told.
    @objc private func systemColorsChanged(_ notification: Notification) {
        strips?.invalidateAll()
        gutterStrips?.invalidateAll()
        if strips == nil { gridView.needsDisplay = true }
        if gutterStrips == nil { gutterView.needsDisplay = true }
        headerView.needsDisplay = true
    }

    @objc private func liveScrollWillStart(_ notification: Notification) {
        scrollInputArrived()
    }

    /// Scrolls the cell a jump ended on back into view if it was in view
    /// before this layout, is still the active cell, and the jump was less
    /// than `revealWindow` ago: the visible area shrank under it (the find
    /// bar or a banner appearing). A cell the user scrolled away from stays
    /// away.
    private func revealJumpedToCell(wasShown: Bool) {
        guard let cell = jumpedTo, cell == activeCell,
              CACurrentMediaTime() - jumpedAt <= Self.revealWindow
        else {
            jumpedTo = nil
            return
        }
        let rect = geometry.cellRect(row: cell.row, column: cell.column)
        if wasShown, !scrollView.documentVisibleRect.contains(rect) {
            scrollToVisible(cell)
        }
    }

    /// The document view is as large as the rows and columns, and at least
    /// as large as the visible area, which is shaded like rows.
    private func updateDocumentSize() {
        let visible = scrollView.contentSize
        let size = NSSize(
            width: max(geometry.totalWidth, visible.width),
            height: max(geometry.height(rows: dataSource?.rowCount ?? 0), visible.height)
        )
        var changed = false
        if gridView.frame.size != size {
            gridView.setFrameSize(size)
            changed = true
        }
        let gutter = NSSize(width: gutterClip.frame.width, height: size.height)
        if gutterView.frame.size != gutter {
            gutterView.setFrameSize(gutter)
            changed = true
        }
        if changed {
            placeStrips()
        }
    }

    // MARK: Strips (ADR-0011)

    /// Strips for a grid no wider than the line (`stripLine`: 1.5 times
    /// the visible width, at most 4,096 pt), AppKit's drawing for a wider
    /// one, switching back only below the line less the hysteresis. Until
    /// the grid is laid out, strips.
    private func updateDrawing() {
        // Not `gridView.frame.width`: when the window narrows, this runs
        // from the scroll view's tiling, before the document view is sized
        // again, so that is the old width against the new visible width.
        // The width it is about to get, against the same visible width.
        let visible = scrollView.contentSize.width
        let width = max(geometry.totalWidth, visible)
        let wanted: Bool
        if !allowsStrips {
            wanted = false
        } else if visible < 1 {
            wanted = drawingChosen ? strips != nil : true
        } else {
            wanted = width <= Self.stripLine(visibleWidth: visible, returning: drawingChosen && strips == nil)
            drawingChosen = true
        }
        guard wanted != (strips != nil) else { return }
        if wanted {
            let strips = GridStrips(content: gridView, widthStep: GridStrips.gridWidthStep)
            strips.view.backingScaleForTesting = stripScaleForTesting
            scrollView.addSubview(strips.view, positioned: .below, relativeTo: overlay)
            self.strips = strips
            gridView.strips = strips
        } else {
            gridView.strips = nil
            strips?.view.removeFromSuperview()
            strips = nil
        }
    }

    /// For tests: the scale strips are drawn at, as if the window were on a
    /// display of that scale (a test can't move its window between
    /// displays); `nil` for the window's own.
    var stripScaleForTesting: CGFloat? {
        didSet {
            for strips in [strips, gutterStrips].compactMap({ $0 }) {
                strips.view.backingScaleForTesting = stripScaleForTesting
                // As AppKit tells views when the window changes display.
                strips.view.viewDidChangeBackingProperties()
            }
        }
    }

    /// The strips' view covers the clip view, and the strips its visible
    /// part; the overlay follows the scroll too. Called on every scroll.
    private func placeStrips() {
        let clip = scrollView.contentView
        // The grid's or the visible area's width may have crossed the line.
        updateDrawing()
        keepStripsUnderScrollers()
        if let strips {
            if strips.view.frame != clip.frame { strips.view.frame = clip.frame }
            strips.update(visible: clip.bounds)
        }
        if overlay.frame != clip.frame { overlay.frame = clip.frame }
        overlay.follow(visible: clip.bounds)
        if let gutterStrips {
            if gutterStrips.view.frame != gutterClip.frame { gutterStrips.view.frame = gutterClip.frame }
            gutterStrips.update(visible: gutterClip.bounds)
        }
    }

    /// The strips' view and the overlay are just above the clip view, in
    /// that order: under the scrollers, which the scroll view puts just
    /// above the clip view when it makes them (after the strips' view).
    private func keepStripsUnderScrollers() {
        let wanted = [strips?.view, overlay].compactMap { $0 }
        let subviews = scrollView.subviews
        guard let clip = subviews.firstIndex(of: scrollView.contentView) else { return }
        let next = subviews[(clip + 1)...].prefix(wanted.count)
        guard !next.elementsEqual(wanted, by: ===) else { return }
        for view in wanted.reversed() {
            scrollView.addSubview(view, positioned: .above, relativeTo: scrollView.contentView)
        }
    }

    /// The values of `rows` (grid rows) changed: an edit, an undo or a
    /// redo (SEAM(2.5)). Their cells and numbers are drawn again, in every
    /// strip that shows them (and, if their ink spilled past them before or
    /// does now, the rows it reaches: `GridView.spills`).
    func cellsChanged(rows: Range<Int>) {
        guard !rows.isEmpty else { return }
        let top = CGFloat(rows.lowerBound) * geometry.rowHeight
        let height = CGFloat(rows.count) * geometry.rowHeight
        // The rows the old values' ink reached are cleared in the same pass.
        let reached = gridView.rows(withSpillsOf: rows)
        gridView.invalidate(CGRect(
            x: 0,
            y: CGFloat(reached.lowerBound) * geometry.rowHeight,
            width: gridView.bounds.width,
            height: CGFloat(reached.count) * geometry.rowHeight
        ))
        gutterView.invalidate(CGRect(x: 0, y: top, width: gutterView.bounds.width, height: height))
    }

    // MARK: Data

    /// The row or column count changed (indexing went on, the estimate
    /// changed), or new rows can be read.
    func reloadData() {
        let rows = dataSource?.rowCount ?? 0
        if GridGutterView.width(rows: rows) != gutterClip.frame.width {
            needsLayout = true
        }
        updateDocumentSize()
        let column = activeCell?.column ?? pendingColumn
        switch pendingJump {
        case .end? where isIndexComplete():
            if rows > 0 { select(CellPosition(row: rows - 1, column: column)) }
            pendingJump = nil
            noteJumpEnded()
        case let .row(target)? where target < (dataSource?.loadedRowCount ?? 0):
            // The target is indexed now (DESIGN §3.10 rule 5).
            select(CellPosition(row: target, column: column))
            noteJumpEnded()
        case .row? where isIndexComplete():
            // The file is shorter than the row asked for: its last row.
            if rows > 0 { select(CellPosition(row: rows - 1, column: column)) }
            pendingJump = nil
            noteJumpEnded()
        default:
            break
        }
        if let current = selection {
            let kept = current.throughLastRow || current.rows.upperBound >= rows
                ? current.clamped(rows: rows, columns: geometry.columnCount)
                : current
            if kept != current { selection = kept }
        }
        gridView.needsDisplay = true
        gutterView.needsDisplay = true
        headerView.needsDisplay = true
        updatePill()
    }

    /// The values changed (the file was read again): forget laid-out text.
    func invalidateContent() {
        gridView.invalidateContent()
        headerView.invalidateContent()
        reloadData()
    }

    // MARK: Scrolling

    /// The rows at least partly in view.
    var visibleRows: Range<Int> {
        let visible = scrollView.contentView.bounds
        return geometry.rowRange(minY: visible.minY, maxY: visible.maxY, rows: dataSource?.rowCount ?? 0)
    }

    /// The user scrolled: a pending ⌘↓ or Go to Row no longer applies.
    func scrollInputArrived() {
        pendingJump = nil
        jumpedTo = nil
        onUserInput?()
    }

    private func followScroll() {
        let origin = scrollView.contentView.bounds.origin
        headerView.offsetX = origin.x
        // Set the origin directly: the grid's may be past the end while it
        // bounces, which the gutter's clip view would otherwise refuse.
        if gutterClip.bounds.origin.y != origin.y {
            gutterClip.setBoundsOrigin(NSPoint(x: 0, y: origin.y))
        }
        placeStrips()
        updatePill()
        onScroll?()
    }

    // MARK: The active cell and keys

    /// Makes `cell` the active cell, the only one selected, and scrolls it
    /// into view. A pending ⌘↓ or Go to Row is dropped: the user chose
    /// another cell.
    func select(_ cell: CellPosition) {
        pendingJump = nil
        jumpedTo = nil
        guard let clamped = clamp(cell) else { return }
        selection = GridSelection(clamped)
        scrollToVisible(clamped)
    }

    /// Selects the cells from the active cell to `cell` (Shift-click, a
    /// drag), keeping the active cell, and scrolls `cell` into view.
    func extend(to cell: CellPosition, throughLastRow: Bool = false) {
        guard let current = selection else {
            select(cell)
            return
        }
        pendingJump = nil
        jumpedTo = nil
        guard let clamped = clamp(cell) else { return }
        selection = current.extended(to: clamped, throughLastRow: throughLastRow)
        scrollToVisible(clamped)
    }

    /// ⌘A: every cell, to the last row however many there turn out to be.
    /// The active cell stays, and nothing scrolls.
    func selectAll() {
        guard let source = dataSource, source.rowCount > 0, geometry.columnCount > 0 else { return }
        let active = activeCell ?? CellPosition(row: 0, column: 0)
        selection = .all(rows: source.rowCount, columns: geometry.columnCount, active: active)
    }

    /// A click on a row number: the row's every cell, with the active cell
    /// in the column it was in.
    func selectRow(_ row: Int) {
        pendingJump = nil
        jumpedTo = nil
        guard let clamped = clamp(CellPosition(row: row, column: activeCell?.column ?? 0)) else { return }
        selection = .row(clamped.row, columns: geometry.columnCount, column: clamped.column)
        scrollToVisible(clamped)
    }

    /// A Shift-click on a row number: every cell of the rows from the
    /// active cell's to `row`.
    func extendRows(to row: Int) {
        guard let current = selection, let clamped = clamp(CellPosition(row: row, column: 0)) else {
            selectRow(row)
            return
        }
        pendingJump = nil
        jumpedTo = nil
        selection = GridSelection(
            active: current.active,
            anchor: CellPosition(row: current.anchor.row, column: 0),
            extent: CellPosition(row: clamped.row, column: geometry.columnCount - 1)
        )
        scrollToVisible(CellPosition(row: clamped.row, column: current.active.column))
    }

    /// `cell`, kept inside the grid; `nil` if the grid has no cells.
    private func clamp(_ cell: CellPosition) -> CellPosition? {
        guard let source = dataSource, source.rowCount > 0, geometry.columnCount > 0 else { return nil }
        return CellPosition(
            row: min(max(0, cell.row), source.rowCount - 1),
            column: min(max(0, cell.column), geometry.columnCount - 1)
        )
    }

    private func scrollToVisible(_ cell: CellPosition) {
        gridView.scrollToVisible(geometry.cellRect(row: cell.row, column: cell.column))
    }

    /// A key's move (DESIGN §4.2). With Shift (`extend`), the selection's
    /// moving corner moves instead, and the active cell stays.
    func move(_ move: GridMove, extend: Bool = false) {
        guard let source = dataSource else { return }
        let rows = source.rowCount
        let columns = geometry.columnCount
        guard rows > 0, columns > 0 else { return }
        let pageRows = Int((scrollView.contentView.bounds.height / geometry.rowHeight).rounded(.down))
        if extend, let current = selection, move != .next, move != .previous {
            let target = move.apply(to: current.extent, rows: rows, columns: columns, pageRows: pageRows)
            // ⇧⌘↓ while indexing selects to the end, however far that is.
            self.extend(to: target, throughLastRow: move == .lastRow && !isIndexComplete())
            return
        }
        let from = activeCell ?? CellPosition(row: 0, column: 0)
        let target = activeCell == nil ? from : move.apply(to: from, rows: rows, columns: columns, pageRows: pageRows)
        select(target)
        // ⌘↓ before the index is complete aims at the estimated last row,
        // which shows skeleton rows and the pill until the real last row
        // is known (mockup 02b). Any other move, click or scroll drops it.
        pendingJump = move == .lastRow && !isIndexComplete() ? .end : nil
        updatePill()
    }

    /// Go to Row (⌘L): selects grid row `row` in the active cell's column.
    /// A row past the indexed ones is shown as soon as the index reaches it
    /// (DESIGN §3.10 rule 5): until then the grid scrolls to where the
    /// estimate puts it, with skeleton rows and the pill. The index is
    /// always running ahead of everything else, on its own thread, so it
    /// is already at the front of the queue.
    func goTo(row: Int) {
        guard let source = dataSource, source.rowCount > 0, geometry.columnCount > 0 else { return }
        let target = max(0, row)
        if target < source.loadedRowCount || isIndexComplete() {
            select(CellPosition(row: target, column: activeCell?.column ?? 0))
            return
        }
        // Scroll to where the row is expected, and wait for it there.
        let estimated = min(target, source.rowCount - 1)
        pendingColumn = activeCell?.column ?? pendingColumn
        selection = nil
        scrollToVisible(CellPosition(row: estimated, column: 0))
        pendingJump = .row(target)
        updatePill()
    }

    // MARK: The indexing pill (mockup 02b)

    private func updatePill() {
        guard let source = dataSource else {
            pill.isHidden = true
            return
        }
        let loaded = source.loadedRowCount
        let visible = visibleRows
        let showing = !isIndexComplete() && !visible.isEmpty && visible.lowerBound >= loaded
        if showing {
            pill.setMessage(GridStrings.pill(jump: pendingJump, row: loaded, of: source.rowCount))
            positionPill()
        }
        if pill.isHidden == showing {
            pill.isHidden = !showing
        }
    }

    private func positionPill() {
        let size = pill.fittingSize
        let area = scrollView.frame
        pill.frame = NSRect(
            x: (area.midX - size.width / 2).rounded(),
            y: (area.midY - size.height / 2).rounded(),
            width: size.width,
            height: size.height
        )
    }
}

/// The floating status pill over skeleton rows: a spinner and "Reaching the
/// end of the file… row 6,110,000 of about 9,400,000" (mockup 02b).
@MainActor
final class IndexingPillView: NSView {
    private let spinner = NSProgressIndicator()
    private let label = NSTextField(labelWithString: "")

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.cornerRadius = 10
        layer?.borderWidth = 1
        layer?.shadowOpacity = 0.12
        layer?.shadowRadius = 8
        layer?.shadowOffset = CGSize(width: 0, height: -2)
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isIndeterminate = true
        label.font = .systemFont(ofSize: 13)
        label.textColor = .labelColor
        let stack = NSStackView(views: [spinner, label])
        stack.orientation = .horizontal
        stack.spacing = 10
        stack.edgeInsets = NSEdgeInsets(top: 10, left: 16, bottom: 10, right: 16)
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)
        // The grid places the pill by its frame, at its fitting size. The
        // trailing and bottom edges aren't required, so that the stack
        // doesn't fight the frame while the pill is hidden at zero size
        // (phase 1 review, app-7).
        let trailing = stack.trailingAnchor.constraint(equalTo: trailingAnchor)
        let bottom = stack.bottomAnchor.constraint(equalTo: bottomAnchor)
        trailing.priority = .init(999)
        bottom.priority = .init(999)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: leadingAnchor),
            trailing,
            stack.topAnchor.constraint(equalTo: topAnchor),
            bottom,
        ])
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    func setMessage(_ message: String) {
        label.stringValue = message
    }

    override var isHidden: Bool {
        didSet {
            if isHidden { spinner.stopAnimation(nil) } else { spinner.startAnimation(nil) }
        }
    }

    override func updateLayer() {
        layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        layer?.borderColor = NSColor.separatorColor.cgColor
    }

    override var wantsUpdateLayer: Bool { true }
}

/// The grid's own words (DESIGN §4.4: from the String Catalog).
enum GridStrings {
    static func pill(jump: GridContainerView.PendingJump?, row: Int, of rows: Int) -> String {
        let row = row.formatted()
        let rows = rows.formatted()
        switch jump {
        case .end?:
            return String(
                localized: "Reaching the end of the file… row \(row) of about \(rows)",
                comment: "Pill over skeleton rows after ⌘↓ before indexing is complete; the indexed row count, then the estimate"
            )
        case let .row(target)?:
            let target = (target + 1).formatted()
            return String(
                localized: "Reaching row \(target)… row \(row) of about \(rows)",
                comment: "Pill over skeleton rows after Go to Row past the indexed rows (⌘L); the row asked for, the indexed row count, then the estimate"
            )
        case nil:
            return String(
                localized: "Indexing… row \(row) of about \(rows)",
                comment: "Pill over skeleton rows scrolled to before they are indexed; the indexed row count, then the estimate"
            )
        }
    }

    /// The title of a column past the header row's last field.
    static func extraColumn(_ number: Int) -> String {
        String(localized: "Column \(number)", comment: "Header of a column only long (ragged) rows have; its 1-based number")
    }
}
