import AppKit

// Option A: a view-based NSTableView. One NSTableColumn per CSV column, cell
// views reused through makeView(withIdentifier:), native sticky header,
// native alternating row colours and vertical grid lines. Cell selection is
// not native (NSTableView selects rows), so the active-cell ring, edited
// triangle and hatching are drawn by a small overlay view that exists only on
// decorated cells. The row-number gutter is the same view B uses.

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

/// Variant "A-lite": a cell view with no NSTextField that draws its own text
/// (same Core Text code as B), so each cell is one view and one layer.
/// The editing cell still uses TableCell, which has a real text field.
final class LiteCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("lite")
    private var text: String?
    private var numeric = false, edited = false, active = false, strong = false
    private var find: NSRange?

    init() {
        super.init(frame: .zero)
        TableCell.created += 1
        identifier = LiteCell.id
        wantsLayer = true
        layerContentsRedrawPolicy = .onSetNeedsDisplay
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
    let table = NSTableView()
    private(set) var drewOnce = false
    private var viewForCalls = 0
    private var editorCell: (row: Int, col: Int)?

    let flatten: Bool
    let lite: Bool

    init(model: DataModel, columns: Columns, flatten: Bool, lite: Bool) {
        self.model = model
        self.columns = columns
        self.flatten = flatten
        self.lite = lite
        super.init()
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
    var counters: [String: Int] { ["viewForCalls": viewForCalls, "cellViewsCreated": TableCell.created] }

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

    /// The standard NSTableView optimisation: a row draws all its cell views
    /// into its own layer instead of each view having a backing store.
    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        guard flatten else { return nil }
        let id = NSUserInterfaceItemIdentifier("row")
        if let v = tableView.makeView(withIdentifier: id, owner: nil) as? NSTableRowView { return v }
        let v = NSTableRowView()
        v.identifier = id
        v.wantsLayer = true
        v.canDrawSubviewsIntoLayer = true
        return v
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        guard let tc = tableColumn as? IndexedColumn else { return nil }
        drewOnce = true
        viewForCalls += 1
        let editing = editorCell.map { $0.row == row && $0.col == tc.index } ?? false
        if lite && !editing {
            let v = (tableView.makeView(withIdentifier: LiteCell.id, owner: nil) as? LiteCell) ?? LiteCell()
            v.configure(model, row: row, col: tc.index)
            return v
        }
        let v = (tableView.makeView(withIdentifier: TableGrid.cellID, owner: nil) as? TableCell) ?? {
            let c = TableCell()
            if flatten {
                // Also flatten each cell (label + decoration) into one layer.
                c.wantsLayer = true
                c.canDrawSubviewsIntoLayer = true
            }
            return c
        }()
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
