import AppKit

// The NSTableView options. All share one NSTableColumn per CSV column, the
// native sticky header and the same gutter as B. Cell selection is not native
// (NSTableView selects rows), so the active-cell ring is drawn by us.
//
//   A       (--impl table): NSTableCellView + NSTextField per cell, native
//           alternating rows and grid lines; ring, triangle and hatching come
//           from a small overlay view that exists only on decorated cells.
//   A-lite  (--impl table-lite): one self-drawing cell view per cell (no
//           NSTextField), flattened into its row view's layer
//           (canDrawSubviewsIntoLayer); the row view draws shading and lines.
//   C       (--impl table-rowdraw): no cell views; each row view draws the
//           row's visible cells itself. NSTableView still virtualises rows and
//           provides the header, column model and row selection.
//
// The editing cell always uses a real NSTextField cell view.

final class IndexedColumn: NSTableColumn {
    var index = 0
}

/// Overlay for the few decorated cells: hatching, corner triangle, ring.
final class DecorationView: NSView {
    var hatched = false, edited = false, active = false
    override var isFlipped: Bool { true }
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        if hatched { Deco.hatch(bounds, ctx) }
        if edited { Deco.triangle(bounds, ctx) }
        if active { Deco.ring(bounds, ctx) }
    }
}

/// A-lite: a cell view with no NSTextField that draws its own text (same Core
/// Text code as B). Its row view flattens all of a row's cells into one layer.
final class LiteCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("lite")
    private var text: String?
    private var numeric = false, edited = false, active = false, strong = false
    private var find: NSRange?

    /// No `wantsLayer` here: a view that asks for its own layer opts out of
    /// its row view's canDrawSubviewsIntoLayer, which is the point of A-lite.
    init() {
        super.init(frame: .zero)
        TableCell.created += 1
        identifier = LiteCell.id
    }
    required init?(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { true }

    func configure(_ m: DataModel, row: Int, col: Int) {
        text = m.cell(row, col)
        numeric = m.numeric(col)
        edited = m.isEdited(row, col)
        active = m.isActive(row, col)
        find = text.flatMap { m.findRange($0) }
        strong = find != nil && m.isCurrentFind(row, col)
        needsDisplay = true
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        if let text {
            Text.draw(text, in: bounds, font: Text.font, color: Palette.text.cgColor, rightAlign: numeric,
                      ctx: ctx, highlight: find, strong: strong)
        } else {
            Deco.hatch(bounds, ctx)
        }
        if edited { Deco.triangle(bounds, ctx) }
        if active { Deco.ring(bounds, ctx) }
    }

    // VoiceOver still gets the value without an NSTextField.
    override func accessibilityValue() -> Any? { text }
}

/// NSTableView whose prepared (overdraw) area can be clamped to the visible
/// rect (--clamp-prepare), to check whether overdraw explains the view count.
final class SpikeTable: NSTableView {
    var clamp = false
    override func prepareContent(in rect: NSRect) {
        super.prepareContent(in: clamp ? visibleRect : rect)
    }
}

/// A-lite's row view: draws shading and grid lines, and flattens its
/// self-drawing cell views into its own layer.
final class ShadeRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("shade")
    var model: DataModel!
    var columns: Columns!
    var row = 0
    override var isOpaque: Bool { true }
    override func drawBackground(in dirtyRect: NSRect) {
        let shades = model.styling ? Palette.rowColors : [Palette.rowColors[0]]
        shades[row % shades.count].setFill()
        dirtyRect.fill()
        Palette.grid.setFill()
        for col in columns.range(dirtyRect.minX, dirtyRect.maxX) {
            NSRect(x: columns.xs[col + 1] - 1, y: dirtyRect.minY, width: 1, height: dirtyRect.height).fill()
        }
    }
    override func drawSelection(in dirtyRect: NSRect) {}
}

/// C's row view: draws the row's visible cells itself (the same drawing as
/// B's grid, one row at a time). No cell views at all.
final class RowDrawView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("rowdraw")
    static var created = 0
    var model: DataModel!
    var columns: Columns!
    var row = 0
    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }
    override func drawBackground(in dirtyRect: NSRect) {
        let shades = model.styling ? Palette.rowColors : [Palette.rowColors[0]]
        shades[row % shades.count].setFill()
        dirtyRect.fill()
    }
    override func drawSelection(in dirtyRect: NSRect) {}
    override func drawSeparator(in dirtyRect: NSRect) {}
    override func draw(_ dirty: NSRect) {
        super.draw(dirty)  // background
        guard let ctx = NSGraphicsContext.current?.cgContext, let model, let columns else { return }
        let cr = columns.range(dirty.minX, dirty.maxX)
        let textColor = Palette.text.cgColor
        for col in cr {
            let r = CGRect(x: columns.xs[col], y: 0, width: columns.widths[col], height: bounds.height)
            guard let s = model.cell(row, col) else { Deco.hatch(r, ctx); continue }
            let hl = model.findRange(s)
            Text.draw(s, in: r, font: Text.font, color: textColor, rightAlign: model.numeric(col), ctx: ctx,
                      highlight: hl, strong: hl != nil && model.isCurrentFind(row, col))
            if model.isEdited(row, col) { Deco.triangle(r, ctx) }
        }
        ctx.setFillColor(Palette.grid.cgColor)
        for col in cr { ctx.fill(CGRect(x: columns.xs[col + 1] - 1, y: dirty.minY, width: 1, height: dirty.height)) }
        if model.styling && row == model.activeRow {
            let c = model.activeCol
            Deco.ring(CGRect(x: columns.xs[c], y: 0, width: columns.widths[c], height: bounds.height), ctx)
        }
    }
}

final class TableCell: NSTableCellView {
    static var created = 0
    let label = NSTextField(labelWithString: "")
    private var deco: DecorationView?

    init() {
        super.init(frame: .zero)
        TableCell.created += 1
        identifier = TableGrid.cellID
        label.font = Text.font
        label.lineBreakMode = .byTruncatingTail
        label.maximumNumberOfLines = 1
        label.cell?.usesSingleLineMode = true
        label.cell?.truncatesLastVisibleLine = true
        addSubview(label)
        textField = label
    }
    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        let h: CGFloat = 17
        // NSTextField insets its text by 2 pt, so this lines up with B's padding.
        label.frame = NSRect(x: Columns.pad - 3, y: (bounds.height - h) / 2,
                             width: bounds.width - 2 * Columns.pad + 6, height: h)
        deco?.frame = bounds
    }

    func configure(_ m: DataModel, row: Int, col: Int, editing: Bool) {
        let s = m.cell(row, col)
        let numeric = m.numeric(col)
        label.alignment = numeric ? .right : .left
        if let s, let r = m.findRange(s) {
            let a = NSMutableAttributedString(string: s, attributes: [.font: Text.font, .foregroundColor: Palette.text])
            a.addAttribute(.backgroundColor,
                           value: m.isCurrentFind(row, col) ? Palette.findStrong.withAlphaComponent(0.55) : Palette.findFill,
                           range: r)
            label.attributedStringValue = a
        } else {
            label.stringValue = s ?? ""
        }
        label.isEditable = editing

        let hatched = s == nil, edited = m.isEdited(row, col), active = m.isActive(row, col)
        if hatched || edited || active {
            if deco == nil {
                let d = DecorationView(frame: bounds)
                addSubview(d)
                deco = d
            }
            deco!.hatched = hatched
            deco!.edited = edited
            deco!.active = active
            deco!.isHidden = false
            deco!.needsDisplay = true
        } else {
            deco?.isHidden = true
        }
    }
}

final class TableGrid: NSObject, GridImpl, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate {
    static let cellID = NSUserInterfaceItemIdentifier("cell")
    let model: DataModel
    let columns: Columns
    let scrollView = NSScrollView()
    let table = SpikeTable()
    private(set) var drewOnce = false
    private var viewForCalls = 0
    private var editorCell: (row: Int, col: Int)?

    enum Mode { case cells, lite, rowdraw }
    let mode: Mode

    init(model: DataModel, columns: Columns, mode: Mode, clampPrepare: Bool) {
        self.model = model
        self.columns = columns
        self.mode = mode
        super.init()
        table.clamp = clampPrepare
        table.style = .plain
        table.rowSizeStyle = .custom
        table.rowHeight = Metrics.rowHeight
        table.intercellSpacing = NSSize(width: 1, height: 0)
        table.usesAlternatingRowBackgroundColors = model.styling
        table.gridStyleMask = .solidVerticalGridLineMask
        table.gridColor = Palette.grid
        table.selectionHighlightStyle = .none
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.allowsColumnReordering = false
        table.usesAutomaticRowHeights = false
        if mode != .cells {
            // The row views draw shading and grid lines themselves.
            table.usesAlternatingRowBackgroundColors = false
            table.gridStyleMask = []
            table.intercellSpacing = NSSize(width: 0, height: 0)
        }
        for c in 0..<model.cols {
            let tc = IndexedColumn(identifier: NSUserInterfaceItemIdentifier("c\(c)"))
            tc.index = c
            tc.minWidth = 20
            tc.maxWidth = 2000
            // Full sample width for the text (the 1 pt grid line comes on top),
            // so A truncates exactly where B does.
            tc.width = columns.widths[c]
            tc.headerCell.attributedStringValue = NSAttributedString(
                string: model.header(c), attributes: [.font: Text.headerFont, .foregroundColor: Palette.text])
            table.addTableColumn(tc)
        }
        table.headerView = NSTableHeaderView(frame: NSRect(x: 0, y: 0, width: columns.total, height: Metrics.headerHeight))
        table.dataSource = self
        table.delegate = self
        scrollView.documentView = table
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.automaticallyAdjustsContentInsets = false
    }

    var documentView: NSView { table }
    var counters: [String: Int] {
        ["viewForCalls": viewForCalls, "cellViewsCreated": TableCell.created, "rowDrawViewsCreated": RowDrawView.created]
    }

    func install(in root: NSView, gutter: GutterView, gutterWidth gw: CGFloat) {
        let W = root.bounds.width, H = root.bounds.height
        let hh = table.headerView?.frame.height ?? Metrics.headerHeight, sh = Metrics.statusHeight
        scrollView.frame = NSRect(x: gw, y: sh, width: W - gw, height: H - sh)
        gutter.frame = NSRect(x: 0, y: sh, width: gw, height: H - sh - hh)
        let corner = CornerView(frame: NSRect(x: 0, y: H - hh, width: gw, height: hh))
        root.addSubview(scrollView)
        root.addSubview(gutter)
        root.addSubview(corner)
    }

    func syncOffsets(gutter: GutterView) {
        // The table's content scrolls under its translucent header, so map the
        // gutter's top edge (level with the header's bottom) into the table.
        gutter.offsetY = table.convert(NSPoint.zero, from: gutter).y
    }

    func numberOfRows(in tableView: NSTableView) -> Int { model.rows }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        switch mode {
        case .cells:
            return nil
        case .lite:
            let v = (tableView.makeView(withIdentifier: ShadeRowView.id, owner: nil) as? ShadeRowView) ?? {
                let v = ShadeRowView()
                v.identifier = ShadeRowView.id
                v.model = model
                v.columns = columns
                v.wantsLayer = true
                v.canDrawSubviewsIntoLayer = true  // one layer per row, not per cell
                return v
            }()
            v.row = row
            v.needsDisplay = true
            return v
        case .rowdraw:
            let v = (tableView.makeView(withIdentifier: RowDrawView.id, owner: nil) as? RowDrawView) ?? {
                let v = RowDrawView()
                RowDrawView.created += 1
                v.identifier = RowDrawView.id
                v.model = model
                v.columns = columns
                v.wantsLayer = true
                v.layerContentsRedrawPolicy = .onSetNeedsDisplay
                return v
            }()
            v.row = row
            v.needsDisplay = true
            return v
        }
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let tc = tableColumn as? IndexedColumn else { return nil }
        drewOnce = true
        viewForCalls += 1
        let editing = editorCell.map { $0.row == row && $0.col == tc.index } ?? false
        if mode == .rowdraw && !editing { return nil }  // the row view draws it
        if mode == .lite && !editing {
            let v = (tableView.makeView(withIdentifier: LiteCell.id, owner: nil) as? LiteCell) ?? LiteCell()
            v.configure(model, row: row, col: tc.index)
            return v
        }
        let v = (tableView.makeView(withIdentifier: TableGrid.cellID, owner: nil) as? TableCell) ?? TableCell()
        v.configure(model, row: row, col: tc.index, editing: editing)
        if editing { v.label.delegate = self }
        return v
    }

    func openEditor(row: Int, col: Int) {
        endEditing()
        model.activeRow = row
        model.activeCol = col
        editorCell = (row, col)
        table.scrollRowToVisible(row)
        table.reloadData(forRowIndexes: IndexSet(integer: row), columnIndexes: IndexSet(integer: col))
        guard let v = table.view(atColumn: col, row: row, makeIfNecessary: true) as? TableCell else { return }
        v.label.isEditable = true
        v.label.delegate = self
        table.window?.makeFirstResponder(v.label)
    }

    func commitEditor(text: String) -> Bool {
        guard let cell = editorCell,
              let v = table.view(atColumn: cell.col, row: cell.row, makeIfNecessary: false) as? TableCell,
              let fe = v.label.currentEditor() as? NSTextView else { return false }
        fe.string = text
        fe.doCommand(by: #selector(NSResponder.insertNewline(_:)))
        return model.cell(cell.row, cell.col) == text && editorCell == nil
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy sel: Selector) -> Bool {
        guard let cell = editorCell else { return false }
        if sel == #selector(NSResponder.insertNewline(_:)) || sel == #selector(NSResponder.insertTab(_:)) {
            model.edits[cell.row * model.cols + cell.col] = textView.string
            endEditing()
            return true
        }
        if sel == #selector(NSResponder.cancelOperation(_:)) {
            endEditing()
            return true
        }
        return false
    }

    private func endEditing() {
        guard let cell = editorCell else { return }
        editorCell = nil
        table.window?.makeFirstResponder(table)
        table.reloadData(forRowIndexes: IndexSet(integer: cell.row), columnIndexes: IndexSet(integer: cell.col))
    }
}
