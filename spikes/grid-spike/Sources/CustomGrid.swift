import AppKit

// Option B: a custom grid. An NSScrollView whose document view is sized to
// the whole table (1M × 22 pt tall) but draws only the cells inside the
// dirty rect, with Core Text. Header and gutter are separate views that
// follow the scroll offset. The in-cell editor is an NSTextField overlaid on
// the cell as a subview of the document view, so it scrolls with the content.

final class GridView: NSView {
    let model: DataModel
    let columns: Columns
    var drewOnce = false
    var drawCalls = 0
    var cellsDrawn = 0

    init(model: DataModel, columns: Columns) {
        self.model = model
        self.columns = columns
        super.init(frame: NSRect(x: 0, y: 0, width: columns.total,
                                 height: CGFloat(model.rows) * Metrics.rowHeight))
        clipsToBounds = true
    }
    required init?(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }
    override var acceptsFirstResponder: Bool { true }

    func cellRect(_ row: Int, _ col: Int) -> CGRect {
        CGRect(x: columns.xs[col], y: CGFloat(row) * Metrics.rowHeight,
               width: columns.widths[col], height: Metrics.rowHeight)
    }

    override func draw(_ dirty: NSRect) {
        drewOnce = true
        drawCalls += 1
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let rh = Metrics.rowHeight
        let first = max(0, Int(dirty.minY / rh))
        let last = min(model.rows - 1, Int((dirty.maxY - 0.01) / rh))
        guard first <= last else { return }
        let cr = columns.range(dirty.minX, dirty.maxX)
        let shades = model.styling ? Palette.rowColors.map(\.cgColor) : [Palette.rowColors[0].cgColor]
        let textColor = Palette.text.cgColor

        for row in first...last {
            ctx.setFillColor(shades[row % shades.count])
            ctx.fill(CGRect(x: dirty.minX, y: CGFloat(row) * rh, width: dirty.width, height: rh))
        }
        for row in first...last {
            for col in cr {
                let r = cellRect(row, col)
                guard let s = model.cell(row, col) else {
                    Deco.hatch(r, ctx)
                    continue
                }
                let hl = model.findRange(s)
                Text.draw(s, in: r, font: Text.font, color: textColor,
                          rightAlign: model.numeric(col), ctx: ctx,
                          highlight: hl, strong: hl != nil && model.isCurrentFind(row, col))
                if model.isEdited(row, col) { Deco.triangle(r, ctx) }
                cellsDrawn += 1
            }
        }
        ctx.setFillColor(Palette.grid.cgColor)
        for col in cr {
            ctx.fill(CGRect(x: columns.xs[col + 1] - 1, y: dirty.minY, width: 1, height: dirty.height))
        }
        if model.styling {
            let ar = cellRect(model.activeRow, model.activeCol)
            if ar.intersects(dirty) { Deco.ring(ar, ctx) }
        }
    }
}

final class HeaderView: NSView {
    let model: DataModel
    let columns: Columns
    var offsetX: CGFloat = 0 {
        didSet { if offsetX != oldValue { needsDisplay = true } }
    }

    init(model: DataModel, columns: Columns) {
        self.model = model
        self.columns = columns
        super.init(frame: .zero)
        clipsToBounds = true
    }
    required init?(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirty: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        Palette.headerBackground.setFill()
        dirty.fill()
        let color = Palette.text.cgColor
        let cr = columns.range(offsetX + dirty.minX, offsetX + dirty.maxX)
        for col in cr {
            let r = CGRect(x: columns.xs[col] - offsetX, y: 0, width: columns.widths[col], height: bounds.height)
            Text.draw(model.header(col), in: r, font: Text.headerFont, color: color, rightAlign: false, ctx: ctx)
            ctx.setFillColor(Palette.grid.cgColor)
            ctx.fill(CGRect(x: r.maxX - 1, y: 0, width: 1, height: bounds.height))
        }
        ctx.setFillColor(Palette.grid.cgColor)
        ctx.fill(CGRect(x: dirty.minX, y: bounds.maxY - 1, width: dirty.width, height: 1))
    }
}

/// Text cell with the grid's horizontal padding, vertically centred.
final class PaddedTextFieldCell: NSTextFieldCell {
    override func drawingRect(forBounds r: NSRect) -> NSRect {
        let h: CGFloat = 17
        return NSRect(x: r.minX + Columns.pad - 2, y: r.minY + (r.height - h) / 2,
                      width: r.width - 2 * Columns.pad + 4, height: h)
    }
}

final class CustomGrid: NSObject, GridImpl, NSTextFieldDelegate {
    let model: DataModel
    let columns: Columns
    let scrollView = NSScrollView()
    let grid: GridView
    let header: HeaderView
    private var editor: NSTextField?
    private var editorCell: (row: Int, col: Int)?

    init(model: DataModel, columns: Columns) {
        self.model = model
        self.columns = columns
        grid = GridView(model: model, columns: columns)
        header = HeaderView(model: model, columns: columns)
        super.init()
        scrollView.documentView = grid
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        scrollView.automaticallyAdjustsContentInsets = false
        scrollView.drawsBackground = false
    }

    var documentView: NSView { grid }
    var drewOnce: Bool { grid.drewOnce }
    var counters: [String: Int] { ["drawCalls": grid.drawCalls, "cellsDrawn": grid.cellsDrawn] }
    func install(in root: NSView, gutter: GutterView, gutterWidth gw: CGFloat) {
        let W = root.bounds.width, H = root.bounds.height
        let hh = Metrics.headerHeight, sh = Metrics.statusHeight
        header.frame = NSRect(x: gw, y: H - hh, width: W - gw, height: hh)
        scrollView.frame = NSRect(x: gw, y: sh, width: W - gw, height: H - sh - hh)
        gutter.frame = NSRect(x: 0, y: sh, width: gw, height: H - sh - hh)
        let corner = CornerView(frame: NSRect(x: 0, y: H - hh, width: gw, height: hh))
        root.addSubview(scrollView)
        root.addSubview(header)
        root.addSubview(gutter)
        root.addSubview(corner)
    }

    func syncOffsets(gutter: GutterView) {
        let v = grid.visibleRect
        gutter.offsetY = v.minY
        header.offsetX = v.minX
    }

    func openEditor(row: Int, col: Int) {
        endEditing()
        model.activeRow = row
        model.activeCol = col
        let r = grid.cellRect(row, col)
        grid.setNeedsDisplay(r.insetBy(dx: -2, dy: -2))
        // Inset by the 2 px ring, which the grid itself draws for the active cell.
        let f = NSTextField(frame: r.insetBy(dx: 2, dy: 2))
        f.cell = PaddedTextFieldCell(textCell: model.cell(row, col) ?? "")
        f.isEditable = true
        f.isBordered = false
        f.isBezeled = false
        f.drawsBackground = true
        f.backgroundColor = .textBackgroundColor
        f.focusRingType = .none
        f.font = Text.font
        f.cell?.usesSingleLineMode = true
        f.cell?.isScrollable = true
        f.delegate = self
        grid.addSubview(f)
        grid.window?.makeFirstResponder(f)
        editor = f
        editorCell = (row, col)
    }

    func commitEditor(text: String) -> Bool {
        guard let f = editor, let cell = editorCell, let fe = f.currentEditor() as? NSTextView else { return false }
        fe.string = text
        // Goes through the same path as pressing Return.
        fe.doCommand(by: #selector(NSResponder.insertNewline(_:)))
        return model.cell(cell.row, cell.col) == text && editor == nil
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
        guard let f = editor, let cell = editorCell else { return }
        editor = nil
        editorCell = nil
        f.removeFromSuperview()
        grid.setNeedsDisplay(grid.cellRect(cell.row, cell.col).insetBy(dx: -2, dy: -2))
        grid.window?.makeFirstResponder(grid)
    }
}
