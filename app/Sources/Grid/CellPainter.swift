import AppKit
import CoreText

/// The grid's fonts (ADR-0002 question 1: the system font at 13 pt).
/// Numbers use tabular digits, so right-aligned columns line up.
@MainActor
enum GridFonts {
    static let cell = NSFont.systemFont(ofSize: 13)
    static let number = NSFont.monospacedDigitSystemFont(ofSize: 13, weight: .regular)
    static let header = NSFont.systemFont(ofSize: 13, weight: .semibold)
    static let gutter = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .regular)
    static let gutterActive = NSFont.monospacedDigitSystemFont(ofSize: 11, weight: .semibold)
}

/// The grid's colours, resolved for one appearance. Every view makes one
/// at the start of `draw(_:)`, when AppKit has set the drawing appearance,
/// so light and dark mode (and Increase Contrast) come from the system
/// colours.
struct GridPalette {
    let rowBackgrounds: [CGColor]
    let text: CGColor
    let secondaryText: CGColor
    let tertiaryText: CGColor
    let gridLine: CGColor
    let accent: CGColor
    let activeFill: CGColor
    let skeleton: CGColor
    let headerBackground: CGColor
    let gutterBackground: CGColor

    /// The palette for the current drawing appearance.
    @MainActor
    static func current() -> GridPalette {
        let dark = NSAppearance.currentDrawing().bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        let backgrounds = NSColor.alternatingContentBackgroundColors.map(\.cgColor)
        let base = backgrounds.first ?? NSColor.controlBackgroundColor.cgColor
        // The mockups' shading is very light (ADR-0002 question 1); the
        // system's second alternating colour is that in light mode, and in
        // dark mode it is drawn over the first.
        let stripe = backgrounds.count > 1 ? backgrounds[1] : base
        return GridPalette(
            rowBackgrounds: [base, dark ? Self.blend(stripe, over: base) : stripe],
            text: NSColor.labelColor.cgColor,
            secondaryText: NSColor.secondaryLabelColor.cgColor,
            tertiaryText: NSColor.tertiaryLabelColor.cgColor,
            gridLine: NSColor.separatorColor.cgColor,
            accent: NSColor.controlAccentColor.cgColor,
            activeFill: NSColor.controlAccentColor.withAlphaComponent(dark ? 0.28 : 0.12).cgColor,
            skeleton: NSColor.labelColor.withAlphaComponent(dark ? 0.12 : 0.08).cgColor,
            headerBackground: dark ? CGColor(gray: 0.15, alpha: 1) : CGColor(gray: 0.985, alpha: 1),
            gutterBackground: dark ? CGColor(gray: 0.13, alpha: 1) : CGColor(gray: 0.96, alpha: 1)
        )
    }

    /// `top` (which may be translucent) drawn over the opaque `bottom`.
    private static func blend(_ top: CGColor, over bottom: CGColor) -> CGColor {
        guard
            let topRGB = top.converted(to: CGColorSpace(name: CGColorSpace.sRGB)!, intent: .defaultIntent, options: nil),
            let bottomRGB = bottom.converted(to: CGColorSpace(name: CGColorSpace.sRGB)!, intent: .defaultIntent, options: nil),
            let t = topRGB.components, let b = bottomRGB.components, t.count == 4, b.count == 4
        else { return top }
        let alpha = t[3]
        return CGColor(
            srgbRed: t[0] * alpha + b[0] * (1 - alpha),
            green: t[1] * alpha + b[1] * (1 - alpha),
            blue: t[2] * alpha + b[2] * (1 - alpha),
            alpha: 1
        )
    }
}

/// A laid-out line of text, kept between frames (ADR-0001: creating a Core
/// Text line for each newly exposed cell was the largest item in the
/// spike's profile).
final class TextLine {
    let line: CTLine
    let width: CGFloat
    /// The line cut with an ellipsis to fit `fittedWidth`, once needed.
    var fitted: CTLine?
    var fittedWidth: CGFloat = -1

    init(_ line: CTLine) {
        self.line = line
        width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
    }
}

/// Laid-out lines for cells, keyed by what they show: the text, whether it
/// is cut short, and the font. Cells with the same value (a column of
/// dates, cities or currencies) share one line, so scrolling makes a new
/// Core Text line only for values it hasn't seen recently.
///
/// It keeps two generations: lookups move a line into the current one,
/// and when the current one is full, the older is dropped whole. So the
/// lines in use stay, and trimming costs nothing per frame. Lines carry
/// their colours, so the cache is cleared when the appearance changes.
@MainActor
final class TextLineCache {
    struct Key: Hashable {
        let text: String
        let truncated: Bool
        let number: Bool
    }

    private var current: [Key: TextLine] = [:]
    private var previous: [Key: TextLine] = [:]
    private let generation: Int

    /// At most about `capacity` lines are kept.
    init(capacity: Int = 1_500) {
        generation = max(1, capacity / 2)
    }

    var count: Int { current.count + previous.count }

    func line(for key: Key, make: () -> TextLine) -> TextLine {
        if let line = current[key] { return line }
        let line = previous.removeValue(forKey: key) ?? make()
        if current.count >= generation {
            // Free the oldest lines off the main thread: releasing hundreds
            // of Core Text lines at once would cost a frame.
            let dropped = Dropped(lines: previous)
            previous = current
            current = [:]
            current.reserveCapacity(generation)
            DispatchQueue.global(qos: .utility).async { _ = dropped }
        }
        current[key] = line
        return line
    }

    func removeAll() {
        current.removeAll()
        previous.removeAll()
    }

    /// Lines nothing else refers to any more, on their way to be freed.
    private struct Dropped: @unchecked Sendable {
        // @unchecked: the lines are only released, and `CTLine` is
        // thread-safe.
        let lines: [Key: TextLine]
    }
}

/// The most characters of a value worth laying out for a column of
/// `width` points: no 13 pt character is narrower than about 3 points, so
/// more can't show. A long value (the core gives up to 256 characters) is
/// cut to this before Core Text sees it, which keeps wide-text columns as
/// cheap to draw as short ones.
func charactersThatFit(width: CGFloat) -> Int {
    max(8, Int((width / 3).rounded(.up)))
}

/// How a cell's text is laid out.
enum CellAlignment {
    case leading
    case trailing
}

/// Draws cells. All the grid's per-cell drawing is here, in one place
/// that fallback C of ADR-0001 (an `NSTableView` whose rows draw their own
/// cells) could reuse unchanged. Every function draws into a flipped
/// context (origin at the top left).
@MainActor
enum CellPainter {
    /// A line of `text` in `font` and `color`, with `symbols` (see
    /// `CellText`) in `symbolColor`.
    static func makeLine(
        _ text: String,
        symbols: [NSRange] = [],
        font: NSFont,
        color: CGColor,
        symbolColor: CGColor
    ) -> TextLine {
        let attributes = attributes(font: font, color: color)
        guard !symbols.isEmpty else {
            // Almost every cell: no symbols, so no mutable string.
            let attributed = CFAttributedStringCreate(nil, text as CFString, attributes)
            return TextLine(CTLineCreateWithAttributedString(attributed!))
        }
        let attributed = NSMutableAttributedString(string: text, attributes: attributes as? [NSAttributedString.Key: Any])
        for range in symbols {
            attributed.addAttribute(colorKey, value: symbolColor, range: range)
        }
        return TextLine(CTLineCreateWithAttributedString(attributed))
    }

    private static let colorKey = NSAttributedString.Key(kCTForegroundColorAttributeName as String)
    /// Attribute dictionaries by font and colour, made once rather than for
    /// every line.
    private static var attributeCache: [AttributesKey: CFDictionary] = [:]

    private struct AttributesKey: Hashable {
        let font: NSFont
        let color: CGColor
    }

    private static func attributes(font: NSFont, color: CGColor) -> CFDictionary {
        let key = AttributesKey(font: font, color: color)
        if let cached = attributeCache[key] { return cached }
        if attributeCache.count > 64 { attributeCache.removeAll() }
        let attributes = [kCTFontAttributeName: font, kCTForegroundColorAttributeName: color] as CFDictionary
        attributeCache[key] = attributes
        return attributes
    }

    /// The line for a cell's value: control characters as symbols, and an
    /// ellipsis if the core gave only its start.
    static func makeCellLine(
        _ value: String,
        truncated: Bool,
        font: NSFont,
        palette: GridPalette,
        color: CGColor? = nil
    ) -> TextLine {
        let (text, symbols) = CellText.display(value)
        return makeLine(
            truncated ? text + "…" : text,
            symbols: symbols,
            font: font,
            color: color ?? palette.text,
            symbolColor: palette.secondaryText
        )
    }

    /// The alternating row shading (ADR-0002 question 1) across `rect`.
    static func drawRowBackground(row: Int, in rect: CGRect, palette: GridPalette, context: CGContext) {
        context.setFillColor(palette.rowBackgrounds[row % palette.rowBackgrounds.count])
        context.fill(rect)
    }

    /// One line of text in a cell: padded, vertically centred, cut with an
    /// ellipsis if it doesn't fit, and aligned.
    static func drawText(
        _ line: TextLine,
        in rect: CGRect,
        font: NSFont,
        alignment: CellAlignment,
        context: CGContext,
        ellipsisColor: CGColor
    ) {
        let available = rect.width - 2 * GridMetrics.cellPadding
        guard available > 2, line.width > 0 else { return }
        var drawn = line.line
        var width = line.width
        if width > available {
            if line.fittedWidth != available {
                let ellipsis = makeLine("…", font: font, color: ellipsisColor, symbolColor: ellipsisColor)
                line.fitted = CTLineCreateTruncatedLine(line.line, Double(available), .end, ellipsis.line)
                line.fittedWidth = available
            }
            guard let fitted = line.fitted else { return }
            drawn = fitted
            width = CGFloat(CTLineGetTypographicBounds(fitted, nil, nil, nil))
        }
        let x = switch alignment {
        case .leading: rect.minX + GridMetrics.cellPadding
        case .trailing: rect.maxX - GridMetrics.cellPadding - width
        }
        let ascent = font.ascender
        let descent = -font.descender
        let baseline = (rect.minY + (rect.height - (ascent + descent)) / 2 + ascent).rounded()
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        context.textPosition = CGPoint(x: x, y: baseline)
        CTLineDraw(drawn, context)
    }

    /// A skeleton cell (mockup 02b): a rounded bar whose length varies from
    /// cell to cell, for rows not read yet.
    static func drawSkeleton(row: Int, column: Int, in rect: CGRect, alignment: CellAlignment, palette: GridPalette, context: CGContext) {
        let available = rect.width - 2 * GridMetrics.cellPadding
        guard available > 6 else { return }
        // A cheap, stable hash: the same cell always gets the same length.
        var hash = UInt64(truncatingIfNeeded: row) &* 0x9E37_79B9_7F4A_7C15 ^ UInt64(truncatingIfNeeded: column) &* 0xC2B2_AE3D_27D4_EB4F
        hash ^= hash >> 29
        let fraction = 0.45 + Double(hash % 1000) / 1000 * 0.45
        let width = (available * fraction).rounded()
        let height: CGFloat = 8
        let x = alignment == .trailing ? rect.maxX - GridMetrics.cellPadding - width : rect.minX + GridMetrics.cellPadding
        let bar = CGRect(x: x, y: rect.midY - height / 2, width: width, height: height)
        context.setFillColor(palette.skeleton)
        context.addPath(CGPath(roundedRect: bar, cornerWidth: height / 2, cornerHeight: height / 2, transform: nil))
        context.fillPath()
    }

    /// The active cell (ADR-0002 question 3): a light accent fill and a 2 pt
    /// accent ring.
    static func drawActiveCellFill(in rect: CGRect, palette: GridPalette, context: CGContext) {
        context.setFillColor(palette.activeFill)
        context.fill(rect)
    }

    static func drawActiveCellRing(in rect: CGRect, palette: GridPalette, context: CGContext) {
        context.setStrokeColor(palette.accent)
        context.setLineWidth(2)
        context.stroke(rect.insetBy(dx: 1, dy: 1))
    }

    /// Find's highlights in a cell (mockup 04a): a yellow mark behind each
    /// match of the query in `line` (the cell's text as `drawText` lays it
    /// out), and for the current match a stronger mark with an orange
    /// outline. `ranges` are UTF-16 ranges of the line's text. Only the
    /// find bar's matches are drawn, so the colours are resolved here, not
    /// in the per-frame palette.
    static func drawFindHighlights(
        _ line: TextLine,
        ranges: [NSRange],
        in rect: CGRect,
        alignment: CellAlignment,
        current: Bool,
        context: CGContext
    ) {
        let available = rect.width - 2 * GridMetrics.cellPadding
        guard available > 2, line.width > 0, !ranges.isEmpty else { return }
        let width = min(line.width, available)
        let x = switch alignment {
        case .leading: rect.minX + GridMetrics.cellPadding
        case .trailing: rect.maxX - GridMetrics.cellPadding - width
        }
        let dark = NSAppearance.currentDrawing().bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        let fill = NSColor.systemYellow.withAlphaComponent(dark ? (current ? 0.55 : 0.32) : (current ? 0.6 : 0.38)).cgColor
        context.saveGState()
        // A long value is cut with an ellipsis where the column ends.
        context.clip(to: CGRect(x: x - 2, y: rect.minY, width: width + 4, height: rect.height))
        for range in ranges {
            let start = CTLineGetOffsetForStringIndex(line.line, range.location, nil)
            let end = CTLineGetOffsetForStringIndex(line.line, range.location + range.length, nil)
            guard end > start else { continue }
            let mark = CGRect(x: x + start - 1, y: rect.minY + 3, width: end - start + 2, height: rect.height - 6)
            let path = CGPath(roundedRect: mark, cornerWidth: 3, cornerHeight: 3, transform: nil)
            context.setFillColor(fill)
            context.addPath(path)
            context.fillPath()
            if current {
                context.setStrokeColor(NSColor.systemOrange.cgColor)
                context.setLineWidth(1.5)
                context.addPath(CGPath(roundedRect: mark.insetBy(dx: 0.75, dy: 0.75), cornerWidth: 3, cornerHeight: 3, transform: nil))
                context.strokePath()
            }
        }
        context.restoreGState()
    }

    /// The vertical line at a column's right edge.
    static func drawColumnSeparator(atX x: CGFloat, minY: CGFloat, maxY: CGFloat, palette: GridPalette, context: CGContext) {
        context.setFillColor(palette.gridLine)
        context.fill(CGRect(x: x - 1, y: minY, width: 1, height: maxY - minY))
    }

    /// A short ragged row's missing cell (ADR-0002 question 5, mockup 03a):
    /// thin diagonal lines. Only ragged rows have these, so the colour is
    /// resolved here rather than in the palette every frame.
    static func drawHatch(in rect: CGRect, context: CGContext) {
        let dark = NSAppearance.currentDrawing().bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        context.saveGState()
        context.clip(to: rect.insetBy(dx: 0, dy: 1))
        context.setStrokeColor(NSColor.labelColor.withAlphaComponent(dark ? 0.16 : 0.12).cgColor)
        context.setLineWidth(1)
        let spacing: CGFloat = 6
        var x = rect.minX - rect.height
        while x < rect.maxX {
            context.move(to: CGPoint(x: x, y: rect.maxY))
            context.addLine(to: CGPoint(x: x + rect.height, y: rect.minY))
            x += spacing
        }
        context.strokePath()
        context.restoreGState()
    }

    /// The gutter's marker for a row with a warning or an error (ADR-0002
    /// question 7, mockup 03a): an orange dot at the gutter's leading edge.
    static func drawGutterMarker(rowRect rect: CGRect, context: CGContext) {
        let size: CGFloat = 6
        let dot = CGRect(x: rect.minX + 6, y: rect.midY - size / 2, width: size, height: size)
        context.setFillColor(NSColor.systemOrange.cgColor)
        context.fillEllipse(in: dot)
    }
}
