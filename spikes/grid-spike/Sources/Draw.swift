import AppKit
import CoreText

// Drawing helpers shared by the custom grid (B), the row-number gutter (both)
// and the decoration overlay used by the NSTableView cells (A).
// Every helper assumes a flipped (top-left origin) context.

enum Text {
    static let font = NSFont.systemFont(ofSize: 13)
    static let headerFont = NSFont.systemFont(ofSize: 13, weight: .semibold)
    static let gutterFont = NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .regular)
    static let gutterBold = NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .semibold)
    static let fgKey = NSAttributedString.Key(kCTForegroundColorAttributeName as String)

    static func line(_ s: String, font: NSFont, color: CGColor) -> CTLine {
        CTLineCreateWithAttributedString(
            NSAttributedString(string: s, attributes: [.font: font, fgKey: color]))
    }

    static func width(_ s: String, font: NSFont) -> CGFloat {
        let l = CTLineCreateWithAttributedString(NSAttributedString(string: s, attributes: [.font: font]))
        return CGFloat(CTLineGetTypographicBounds(l, nil, nil, nil))
    }

    private static var ellipses: [CGColor: CTLine] = [:]
    private static func ellipsis(_ font: NSFont, _ color: CGColor) -> CTLine {
        if let l = ellipses[color] { return l }
        let l = line("\u{2026}", font: font, color: color)
        ellipses[color] = l
        return l
    }

    /// Draws one line of text inside `rect`, padded, truncated with an
    /// ellipsis, left or right aligned. Optionally paints a find-match
    /// highlight behind the matched range first.
    static func draw(_ s: String, in rect: CGRect, font: NSFont, color: CGColor,
                     rightAlign: Bool, ctx: CGContext,
                     highlight: NSRange? = nil, strong: Bool = false) {
        if s.isEmpty { return }
        let avail = rect.width - 2 * Columns.pad
        guard avail > 4 else { return }
        var l = line(s, font: font, color: color)
        var w = CGFloat(CTLineGetTypographicBounds(l, nil, nil, nil))
        if w > avail, let t = CTLineCreateTruncatedLine(l, Double(avail), .end, ellipsis(font, color)) {
            l = t
            w = CGFloat(CTLineGetTypographicBounds(t, nil, nil, nil))
        }
        let x = rightAlign ? rect.maxX - Columns.pad - w : rect.minX + Columns.pad
        let ascent = font.ascender, descent = -font.descender
        let baseline = rect.minY + (rect.height - (ascent + descent)) / 2 + ascent

        if let r = highlight {
            let x1 = CTLineGetOffsetForStringIndex(l, r.location, nil)
            var x2 = CTLineGetOffsetForStringIndex(l, r.location + r.length, nil)
            if x2 <= x1 { x2 = w }  // match truncated away: highlight to the end
            let hr = CGRect(x: x + x1 - 1.5, y: rect.minY + 3, width: x2 - x1 + 3, height: rect.height - 6)
            let path = CGPath(roundedRect: hr, cornerWidth: 3, cornerHeight: 3, transform: nil)
            ctx.addPath(path)
            ctx.setFillColor(Palette.findFill.cgColor)
            ctx.fillPath()
            if strong {
                ctx.addPath(path)
                ctx.setStrokeColor(Palette.findStrong.cgColor)
                ctx.setLineWidth(1.5)
                ctx.strokePath()
            }
        }
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        ctx.textPosition = CGPoint(x: x, y: baseline)
        CTLineDraw(l, ctx)
    }
}

enum Palette {
    static let text = NSColor.labelColor
    static let secondary = NSColor.secondaryLabelColor
    static let accent = NSColor.controlAccentColor
    static let grid = NSColor.gridColor
    static let hatch = NSColor.tertiaryLabelColor
    static let diag = NSColor.systemOrange
    static let findFill = NSColor.systemYellow.withAlphaComponent(0.45)
    static let findStrong = NSColor.systemOrange
    static var rowColors: [NSColor] { NSColor.alternatingContentBackgroundColors }
    static let gutterBackground = NSColor(name: nil) { a in
        a.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(white: 0.14, alpha: 1) : NSColor(white: 0.965, alpha: 1)
    }
    static let headerBackground = NSColor.controlBackgroundColor
}

enum Deco {
    /// Diagonal hatching for a missing (ragged) cell.
    static func hatch(_ r: CGRect, _ ctx: CGContext) {
        ctx.saveGState()
        ctx.clip(to: r)
        ctx.setStrokeColor(Palette.hatch.withAlphaComponent(0.35).cgColor)
        ctx.setLineWidth(1)
        var x = r.minX - r.height
        while x < r.maxX {
            ctx.move(to: CGPoint(x: x, y: r.maxY))
            ctx.addLine(to: CGPoint(x: x + r.height, y: r.minY))
            x += 6
        }
        ctx.strokePath()
        ctx.restoreGState()
    }

    /// Accent corner triangle for an edited-but-unsaved cell.
    static func triangle(_ r: CGRect, _ ctx: CGContext) {
        ctx.setFillColor(Palette.accent.cgColor)
        ctx.move(to: CGPoint(x: r.maxX - 8, y: r.minY))
        ctx.addLine(to: CGPoint(x: r.maxX, y: r.minY))
        ctx.addLine(to: CGPoint(x: r.maxX, y: r.minY + 8))
        ctx.closePath()
        ctx.fillPath()
    }

    /// 2 px accent ring on the active cell.
    static func ring(_ r: CGRect, _ ctx: CGContext) {
        ctx.setStrokeColor(Palette.accent.cgColor)
        ctx.setLineWidth(2)
        ctx.stroke(r.insetBy(dx: 1, dy: 1))
    }
}

/// Row-number gutter, used unchanged by both implementations. It sits outside
/// the scroll view and follows its vertical offset.
final class GutterView: NSView {
    let model: DataModel
    var offsetY: CGFloat = 0 {
        didSet { if offsetY != oldValue { needsDisplay = true } }
    }

    static func width(rows: Int) -> CGFloat {
        ceil(Text.width(String(rows), font: Text.gutterFont)) + 30
    }

    init(model: DataModel) {
        self.model = model
        super.init(frame: .zero)
        clipsToBounds = true
    }
    required init?(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { true }
    override var isOpaque: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        Palette.gutterBackground.setFill()
        dirtyRect.fill()
        Palette.grid.setFill()
        NSRect(x: bounds.maxX - 1, y: dirtyRect.minY, width: 1, height: dirtyRect.height).fill()

        let rh = Metrics.rowHeight
        let first = max(0, Int((offsetY + dirtyRect.minY) / rh))
        let last = min(model.rows - 1, Int((offsetY + dirtyRect.maxY) / rh))
        guard first <= last else { return }
        let normal = Palette.secondary.cgColor, active = Palette.accent.cgColor
        for row in first...last {
            let y = CGFloat(row) * rh - offsetY
            let r = CGRect(x: 0, y: y, width: bounds.width - 4, height: rh)
            let isActive = model.styling && row == model.activeRow
            Text.draw(String(row + 1), in: r, font: isActive ? Text.gutterBold : Text.gutterFont,
                      color: isActive ? active : normal, rightAlign: true, ctx: ctx)
            if model.hasDiagnostic(row) {
                ctx.setFillColor(Palette.diag.cgColor)
                ctx.fillEllipse(in: CGRect(x: 7, y: y + rh / 2 - 3, width: 6, height: 6))
            }
        }
    }
}

/// Plain header-coloured box, used above the gutter.
final class CornerView: NSView {
    override var isFlipped: Bool { true }
    override func draw(_ dirtyRect: NSRect) {
        Palette.headerBackground.setFill()
        bounds.fill()
        Palette.grid.setFill()
        NSRect(x: 0, y: bounds.maxY - 1, width: bounds.width, height: 1).fill()
        NSRect(x: bounds.maxX - 1, y: 0, width: 1, height: bounds.height).fill()
    }
}

func makeStatusBar(_ model: DataModel) -> NSView {
    let v = NSView()
    let f = NumberFormatter()
    f.numberStyle = .decimal
    let rows = f.string(from: NSNumber(value: model.rows)) ?? "\(model.rows)"
    let label = NSTextField(labelWithString: "\(rows) rows × \(model.cols) columns  ·  Comma  ·  CRLF  ·  UTF-8 (BOM)")
    label.font = NSFont.systemFont(ofSize: 11)
    label.textColor = .secondaryLabelColor
    label.frame = NSRect(x: 12, y: 4, width: 600, height: 16)
    v.addSubview(label)
    return v
}
