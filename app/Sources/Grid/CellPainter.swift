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
    /// Find's marks (mockup 04a): behind a match, behind the current one,
    /// and the current one's outline.
    let findMark: CGColor
    let findCurrentMark: CGColor
    let findCurrentOutline: CGColor
    /// The lines of a missing cell's hatching.
    let hatch: CGColor

    /// The palette for the current drawing appearance. Made once for each
    /// appearance, and again when the system's colours change (the accent
    /// colour, for one): every view draws with it on every frame of a
    /// scroll (task 2.0a).
    @MainActor
    static func current() -> GridPalette {
        let appearance = NSAppearance.currentDrawing().name
        if let cached = PaletteCache.palette, PaletteCache.appearance == appearance {
            return cached
        }
        PaletteCache.observe()
        let palette = make()
        PaletteCache.appearance = appearance
        PaletteCache.palette = palette
        return palette
    }

    @MainActor
    private static func make() -> GridPalette {
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
            gutterBackground: dark ? CGColor(gray: 0.13, alpha: 1) : CGColor(gray: 0.96, alpha: 1),
            findMark: NSColor.systemYellow.withAlphaComponent(dark ? 0.32 : 0.38).cgColor,
            findCurrentMark: NSColor.systemYellow.withAlphaComponent(dark ? 0.55 : 0.6).cgColor,
            findCurrentOutline: NSColor.systemOrange.cgColor,
            hatch: NSColor.labelColor.withAlphaComponent(dark ? 0.16 : 0.12).cgColor
        )
    }

    /// The palette last made, and for which appearance.
    @MainActor
    private enum PaletteCache {
        static var appearance: NSAppearance.Name?
        static var palette: GridPalette?
        private static var observers: [NSObjectProtocol] = []

        /// The system's colours change with the accent colour and the
        /// highlight colour, and with Increase Contrast, which is an
        /// accessibility display option.
        static func observe() {
            guard observers.isEmpty else { return }
            let forget: @Sendable (Notification) -> Void = { _ in
                MainActor.assumeIsolated { palette = nil }
            }
            observers = [
                NotificationCenter.default.addObserver(forName: NSColor.systemColorsDidChangeNotification, object: nil, queue: .main, using: forget),
                NSWorkspace.shared.notificationCenter.addObserver(
                    forName: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification,
                    object: nil,
                    queue: .main,
                    using: forget
                ),
            ]
        }
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

/// A line laid out by Core Text, with what drawing it needs, made on any
/// thread and never changed after (task 2.0a): the main thread keeps it in
/// a `TextLine`. Lines laid out ahead of the scroll (`LineReadAhead`) are
/// made off the main thread and handed over as this.
struct LaidOutLine: @unchecked Sendable {
    // @unchecked: every stored property is a `let` of an immutable value;
    // Core Text's lines and fonts and Core Graphics' colours are immutable
    // and documented as safe to use from any thread, but not marked
    // Sendable.
    /// A cut of the line to a column's width, with an ellipsis.
    struct Fitted {
        /// The width it was cut to.
        let available: CGFloat
        let line: CTLine?
        let width: CGFloat
        let runs: [GlyphRun]?
    }

    let line: CTLine
    let width: CGFloat
    /// The line's glyphs, as `CTLineDraw` draws them (`nil` if it has a
    /// run they can't stand for).
    let runs: [GlyphRun]?
    let fitted: Fitted?
    /// `TextLine.offset(at:)`'s table, if it was asked for.
    let caretOffsets: [CGFloat]?
    /// The ink's bounds, from the line's origin, y going up: from the
    /// glyphs' boxes as laid out, stacked marks included (task 2.0b: ink
    /// can spill past its row).
    let ink: CGRect

    init(_ line: CTLine) {
        self.line = line
        width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
        runs = GlyphRun.runs(of: line)
        fitted = nil
        caretOffsets = nil
        ink = CTLineGetImageBounds(line, nil)
    }

    private init(_ other: LaidOutLine, fitted: Fitted?, caretOffsets: [CGFloat]?) {
        line = other.line
        width = other.width
        ink = other.ink
        runs = other.runs
        self.fitted = fitted
        self.caretOffsets = caretOffsets
    }

    /// This line with its cut to a column and its caret offsets.
    func adding(fitted: Fitted?, caretOffsets: [CGFloat]?) -> LaidOutLine {
        LaidOutLine(self, fitted: fitted, caretOffsets: caretOffsets)
    }

    /// `line` cut with `ellipsis` to fit `available` points, as
    /// `CTLineCreateTruncatedLine` cuts it.
    static func fit(_ line: CTLine, _ available: CGFloat, ellipsis: CTLine) -> Fitted {
        let fitted = CTLineCreateTruncatedLine(line, Double(available), .end, ellipsis)
        return Fitted(
            available: available,
            line: fitted,
            width: fitted.map { CGFloat(CTLineGetTypographicBounds($0, nil, nil, nil)) } ?? 0,
            runs: fitted.flatMap(GlyphRun.runs(of:))
        )
    }

    /// The caret's offset before each UTF-16 unit of `line`'s text, as
    /// `CTLineGetOffsetForStringIndex` gives it, found in one pass (task
    /// 2.0a: asking for each end of each find mark took a tenth of the main
    /// thread's time while a search's matches showed). `nan` where Core
    /// Text has no caret of its own (inside a cluster); the last is the
    /// end of the text.
    static func caretOffsets(of line: CTLine) -> [CGFloat]? {
        // Right-to-left text, alone or mixed with left-to-right, has carets
        // whose primary offsets this pass doesn't reproduce: each is asked
        // for on its own.
        let runs = CTLineGetGlyphRuns(line) as? [CTRun] ?? []
        guard !runs.contains(where: { CTRunGetStatus($0).contains(.rightToLeft) }) else { return nil }
        let length = CTLineGetStringRange(line).length
        var offsets = [CGFloat](repeating: .nan, count: length + 1)
        CTLineEnumerateCaretOffsets(line) { offset, index, leadingEdge, _ in
            // The leading edge of a unit is the caret before it, as
            // `CTLineGetOffsetForStringIndex` reports it (its primary
            // offset); the first edge seen is kept.
            guard leadingEdge, index >= 0, index < length, offsets[index].isNaN else { return }
            offsets[index] = offset
        }
        offsets[length] = CTLineGetOffsetForStringIndex(line, length, nil)
        return offsets
    }
}

/// A laid-out line of text, kept between frames (ADR-0001: creating a Core
/// Text line for each newly exposed cell was the largest item in the
/// spike's profile). Only the main thread uses one.
final class TextLine {
    let line: CTLine
    let width: CGFloat
    /// The line's glyphs, as `CTLineDraw` draws them (`nil` if it has a
    /// run they can't stand for).
    let runs: [GlyphRun]?
    /// The line cut with an ellipsis to fit a column, once needed.
    private(set) var fitted: LaidOutLine.Fitted?
    private var caretOffsets: [CGFloat]?
    private var caretOffsetsMade = false
    /// The ink's bounds (`LaidOutLine.ink`); a cut line's ink is within
    /// them.
    let ink: CGRect

    init(_ laidOut: LaidOutLine) {
        line = laidOut.line
        width = laidOut.width
        ink = laidOut.ink
        runs = laidOut.runs
        fitted = laidOut.fitted
        caretOffsets = laidOut.caretOffsets
        caretOffsetsMade = laidOut.caretOffsets != nil
    }

    convenience init(_ line: CTLine) {
        self.init(LaidOutLine(line))
    }

    /// The offset of the caret before UTF-16 unit `index` of the line's text.
    func offset(at index: Int) -> CGFloat {
        if !caretOffsetsMade {
            caretOffsets = LaidOutLine.caretOffsets(of: line)
            caretOffsetsMade = true
        }
        if let offsets = caretOffsets, index >= 0, index < offsets.count, !offsets[index].isNaN { return offsets[index] }
        return CTLineGetOffsetForStringIndex(line, index, nil)
    }

    /// Cuts the line with `ellipsis` to fit `available` points.
    func fit(_ available: CGFloat, ellipsis: CTLine) {
        fitted = LaidOutLine.fit(line, available, ellipsis: ellipsis)
    }
}

/// One run of a laid-out line: its glyphs, where they go (relative to the
/// line's origin, as Core Text placed them), and in which font and colour.
/// The grid hands many cells' runs to the context at once, one call per
/// font and colour (`GlyphBatch`), instead of one `CTLineDraw` per cell.
struct GlyphRun: @unchecked Sendable {
    // @unchecked: as `LaidOutLine`.
    let font: CTFont
    let color: CGColor
    let glyphs: [CGGlyph]
    let positions: [CGPoint]

    /// The runs of `line`, or `nil` if any run is drawn with more than
    /// glyphs at positions (a text matrix of its own), which only
    /// `CTLineDraw` reproduces.
    static func runs(of line: CTLine) -> [GlyphRun]? {
        let runs = CTLineGetGlyphRuns(line) as? [CTRun] ?? []
        var result: [GlyphRun] = []
        result.reserveCapacity(runs.count)
        for run in runs {
            let count = CTRunGetGlyphCount(run)
            guard count > 0 else { continue }
            guard !CTRunGetStatus(run).contains(.hasNonIdentityMatrix) else { return nil }
            let attributes = CTRunGetAttributes(run) as NSDictionary
            // Core Text puts the run's font (a fallback font, if it took
            // one) in its attributes; the colour is the line's.
            guard let fontValue = attributes[kCTFontAttributeName] as AnyObject?,
                  CFGetTypeID(fontValue) == CTFontGetTypeID()
            else { return nil }
            let font = fontValue as! CTFont
            // Colour glyphs (emoji) are drawn by `CTLineDraw` its own way.
            guard !CTFontGetSymbolicTraits(font).contains(.traitColorGlyphs) else { return nil }
            let colorValue = attributes[kCTForegroundColorAttributeName] as AnyObject?
            // Core Text draws a run without a colour in black.
            let color = colorValue.flatMap { CFGetTypeID($0) == CGColor.typeID ? ($0 as! CGColor) : nil }
                ?? CGColor(gray: 0, alpha: 1)
            var glyphs = [CGGlyph](repeating: 0, count: count)
            var positions = [CGPoint](repeating: .zero, count: count)
            CTRunGetGlyphs(run, CFRange(location: 0, length: count), &glyphs)
            CTRunGetPositions(run, CFRange(location: 0, length: count), &positions)
            result.append(GlyphRun(font: font, color: color, glyphs: glyphs, positions: positions))
        }
        return result
    }
}

/// Glyphs gathered from many cells, drawn with one call per font and
/// colour (task 2.0a). In a scroll view AppKit records the grid's drawing
/// and replays all of what is in view on every frame; one call for a run
/// of cells instead of one per cell leaves it far less to replay. What is
/// drawn is the same: the glyphs `CTLineDraw` would draw, at the same
/// places, in the same order relative to everything else (`draw`).
@MainActor
final class GlyphBatch {
    private struct Group {
        let font: CTFont
        let color: CGColor
        var glyphs: [CGGlyph] = []
        var positions: [CGPoint] = []

        mutating func append(_ run: GlyphRun, x: CGFloat, baseline: CGFloat) {
            glyphs.append(contentsOf: run.glyphs)
            // The context's text matrix flips y (see `draw`), as
            // `CellPainter.drawText`'s does for `CTLineDraw`.
            positions.reserveCapacity(positions.count + run.positions.count)
            for position in run.positions {
                positions.append(CGPoint(x: x + position.x, y: position.y - baseline))
            }
        }

        func holds(font: CTFont, color: CGColor) -> Bool {
            (self.font === font || CFEqual(self.font, font)) && (self.color === color || self.color == color)
        }
    }

    private var groups: [Group] = []
    private var last = 0
    private var pending = false

    /// Adds `runs` with the line's origin at `x` on the baseline `baseline`
    /// (in a flipped context's coordinates), if they are all in one font
    /// and colour; `false` if they aren't. The glyphs of different cells
    /// never overlap, so drawing them together changes no pixel; but a
    /// line's own runs (a value with symbols, or a fallback font) may
    /// touch, so those are drawn in their own order, by `CTLineDraw`.
    func add(_ runs: [GlyphRun], x: CGFloat, baseline: CGFloat) -> Bool {
        guard let first = runs.first else { return true }
        for run in runs.dropFirst() where !(run.font === first.font && (run.color === first.color || run.color == first.color)) {
            return false
        }
        let index = groupIndex(font: first.font, color: first.color)
        for run in runs {
            groups[index].append(run, x: x, baseline: baseline)
        }
        pending = true
        return true
    }

    private func groupIndex(font: CTFont, color: CGColor) -> Int {
        if last < groups.count, groups[last].font === font, groups[last].color === color { return last }
        // The same objects, almost always (the palette and fonts are made
        // once); comparing fonts by value is far slower.
        if let index = groups.firstIndex(where: { $0.font === font && $0.color === color })
            ?? groups.firstIndex(where: { $0.holds(font: font, color: color) })
        {
            last = index
            return index
        }
        groups.append(Group(font: font, color: color))
        last = groups.count - 1
        return last
    }

    /// Draws what was added, and empties the batch (keeping its storage).
    /// The grid draws it before anything else a cell draws (a fill, a
    /// find mark, a skeleton, a hatch, a line drawn on its own), so that
    /// everything is drawn in the order it was before batching: text ink
    /// that spills out of its cell still lies under what comes after it.
    func draw(in context: CGContext) {
        guard pending else { return }
        pending = false
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        for index in groups.indices where !groups[index].glyphs.isEmpty {
            context.setFillColor(groups[index].color)
            CTFontDrawGlyphs(groups[index].font, groups[index].glyphs, groups[index].positions, groups[index].glyphs.count, context)
            groups[index].glyphs.removeAll(keepingCapacity: true)
            groups[index].positions.removeAll(keepingCapacity: true)
        }
        // Few distinct fonts and colours are ever drawn; don't keep the
        // groups of an old palette or a fallback font for ever.
        if groups.count > 16 {
            groups.removeAll()
            last = 0
        }
    }
}

/// What a cell's line is keyed by: the text it shows, whether it is cut
/// short, and the font.
struct CellLineKey: Hashable, Sendable {
    let text: String
    let truncated: Bool
    let number: Bool
}

typealias TextLineCache = LineCache<CellLineKey>

/// Laid-out lines for cells, keyed by what they show: the text, whether it
/// is cut short, and the font. Cells with the same value (a column of
/// dates, cities or currencies) share one line, so scrolling makes a new
/// Core Text line only for values it hasn't seen recently.
///
/// It keeps two generations: lookups move a line into the current one,
/// and when the current one is full, the older is dropped whole. So the
/// lines in use stay, and trimming costs nothing per frame. Lines carry
/// their colours, so the cache is cleared when the appearance changes.
///
/// The gutter keeps its row numbers in one too, keyed by row.
@MainActor
final class LineCache<Key: Hashable & Sendable> {
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

    func contains(_ key: Key) -> Bool {
        current[key] != nil || previous[key] != nil
    }

    /// Keeps `line` for `key`, unless there already is one.
    func insert(_ line: TextLine, for key: Key) {
        _ = self.line(for: key) { line }
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

/// Which line a text cell is drawn with, in a column `width` points wide:
/// only what the column could show is laid out (`charactersThatFit`).
struct LineRequest: Sendable {
    /// The line cache's key for it.
    let key: CellLineKey
    /// The part of the value laid out.
    let shown: String
    /// Whether the line ends with an ellipsis of its own: the core gave
    /// only the value's start, and all of that is shown.
    let ellipsis: Bool

    init(value: String, truncated: Bool, width: CGFloat, number: Bool) {
        let fits = charactersThatFit(width: width)
        let cut = value.utf8.count > fits && value.count > fits
        shown = cut ? String(value.prefix(fits)) : value
        key = CellLineKey(text: shown, truncated: truncated || cut, number: number)
        ellipsis = truncated && !cut
    }
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
        TextLine(makeLine(text, symbols: symbols, attributes: attributes(font: font, color: color), symbolColor: symbolColor))
    }

    /// `makeLine` with its font and colour as Core Text attributes: on any
    /// thread (`LineReadAhead`).
    nonisolated static func makeLine(
        _ text: String,
        symbols: [NSRange],
        attributes: CFDictionary,
        symbolColor: CGColor
    ) -> LaidOutLine {
        guard !symbols.isEmpty else {
            // Almost every cell: no symbols, so no mutable string.
            let attributed = CFAttributedStringCreate(nil, text as CFString, attributes)
            return LaidOutLine(CTLineCreateWithAttributedString(attributed!))
        }
        let attributed = NSMutableAttributedString(string: text, attributes: attributes as? [NSAttributedString.Key: Any])
        for range in symbols {
            attributed.addAttribute(colorKey, value: symbolColor, range: range)
        }
        return LaidOutLine(CTLineCreateWithAttributedString(attributed))
    }

    private nonisolated static let colorKey = NSAttributedString.Key(kCTForegroundColorAttributeName as String)
    /// Attribute dictionaries by font and colour, made once rather than for
    /// every line.
    private static var attributeCache: [AttributesKey: CFDictionary] = [:]

    private struct AttributesKey: Hashable {
        let font: NSFont
        let color: CGColor
    }

    static func attributes(font: NSFont, color: CGColor) -> CFDictionary {
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
        TextLine(makeCellLine(value, truncated: truncated, attributes: attributes(font: font, color: color ?? palette.text), symbolColor: palette.secondaryText))
    }

    /// `makeCellLine` on any thread.
    nonisolated static func makeCellLine(_ value: String, truncated: Bool, attributes: CFDictionary, symbolColor: CGColor) -> LaidOutLine {
        let (text, symbols) = CellText.display(value)
        return makeLine(truncated ? text + "…" : text, symbols: symbols, attributes: attributes, symbolColor: symbolColor)
    }

    /// The alternating row shading (ADR-0002 question 1) of `rows`, from
    /// `minX` across `width`: one fill per colour (task 2.0a), since AppKit
    /// replays every drawing call in view on every scroll step. `scratch`
    /// is storage kept between draws.
    static func drawRowBackgrounds(
        rows: Range<Int>,
        minX: CGFloat,
        width: CGFloat,
        rowHeight: CGFloat,
        palette: GridPalette,
        context: CGContext,
        scratch: inout [CGRect]
    ) {
        let colors = palette.rowBackgrounds
        for (index, color) in colors.enumerated() {
            scratch.removeAll(keepingCapacity: true)
            for row in rows where row % colors.count == index {
                scratch.append(CGRect(x: minX, y: CGFloat(row) * rowHeight, width: width, height: rowHeight))
            }
            context.setFillColor(color)
            context.fill(scratch)
        }
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
        guard let (drawn, x, baseline) = placeText(line, in: rect, font: font, alignment: alignment, ellipsisColor: ellipsisColor) else { return }
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        context.textPosition = CGPoint(x: x, y: baseline)
        CTLineDraw(drawn, context)
    }

    /// Adds a cell's text to `batch`, to be drawn with other cells' (task
    /// 2.0a): placed and cut exactly as `drawText` places and cuts it. A
    /// line whose runs the batch can't stand for is drawn on its own.
    static func addText(
        _ line: TextLine,
        in rect: CGRect,
        font: NSFont,
        alignment: CellAlignment,
        to batch: GlyphBatch,
        context: CGContext,
        ellipsisColor: CGColor
    ) {
        guard let (drawn, x, baseline) = placeText(line, in: rect, font: font, alignment: alignment, ellipsisColor: ellipsisColor) else { return }
        let runs = drawn === line.line ? line.runs : line.fitted?.runs
        if let runs, batch.add(runs, x: x, baseline: baseline) { return }
        // Drawn now, after the text before it, as it always was.
        batch.draw(in: context)
        context.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        context.textPosition = CGPoint(x: x, y: baseline)
        CTLineDraw(drawn, context)
    }

    /// Where a line goes in a cell: the line to draw (cut with an ellipsis
    /// if it doesn't fit), its origin's x and its baseline.
    private static func placeText(
        _ line: TextLine,
        in rect: CGRect,
        font: NSFont,
        alignment: CellAlignment,
        ellipsisColor: CGColor
    ) -> (CTLine, CGFloat, CGFloat)? {
        let available = rect.width - 2 * GridMetrics.cellPadding
        guard available > 2, line.width > 0 else { return nil }
        var drawn = line.line
        var width = line.width
        if width > available {
            if line.fitted?.available != available {
                line.fit(available, ellipsis: makeLine("…", font: font, color: ellipsisColor, symbolColor: ellipsisColor).line)
            }
            guard let fitted = line.fitted, let fittedLine = fitted.line else { return nil }
            drawn = fittedLine
            width = fitted.width
        }
        let x = switch alignment {
        case .leading: rect.minX + GridMetrics.cellPadding
        case .trailing: rect.maxX - GridMetrics.cellPadding - width
        }
        return (drawn, x, baseline(in: rect, font: font))
    }

    /// The baseline of a line of `font` in a cell: vertically centred.
    static func baseline(in rect: CGRect, font: NSFont) -> CGFloat {
        let ascent = font.ascender
        let descent = -font.descender
        return (rect.minY + (rect.height - (ascent + descent)) / 2 + ascent).rounded()
    }

    /// How many rows above and below its own `line`'s ink reaches, drawn
    /// in a cell `rect` (task 2.0b): stacked marks can spill past the row.
    static func inkSpill(of line: TextLine, in rect: CGRect, font: NSFont) -> InkSpill {
        let base = baseline(in: rect, font: font)
        // The ink goes from `base - ink.maxY` down to `base - ink.minY`.
        let over = rect.minY - (base - line.ink.maxY)
        let under = (base - line.ink.minY) - rect.maxY
        return InkSpill(
            above: over > 0 ? Int((over / rect.height).rounded(.up)) : 0,
            below: under > 0 ? Int((under / rect.height).rounded(.up)) : 0
        )
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
        palette: GridPalette,
        context: CGContext
    ) {
        let available = rect.width - 2 * GridMetrics.cellPadding
        guard available > 2, line.width > 0, !ranges.isEmpty else { return }
        let width = min(line.width, available)
        let x = switch alignment {
        case .leading: rect.minX + GridMetrics.cellPadding
        case .trailing: rect.maxX - GridMetrics.cellPadding - width
        }
        let fill = current ? palette.findCurrentMark : palette.findMark
        context.saveGState()
        // A long value is cut with an ellipsis where the column ends.
        context.clip(to: CGRect(x: x - 2, y: rect.minY, width: width + 4, height: rect.height))
        for range in ranges {
            let start = line.offset(at: range.location)
            let end = line.offset(at: range.location + range.length)
            guard end > start else { continue }
            let mark = CGRect(x: x + start - 1, y: rect.minY + 3, width: end - start + 2, height: rect.height - 6)
            let path = CGPath(roundedRect: mark, cornerWidth: 3, cornerHeight: 3, transform: nil)
            context.setFillColor(fill)
            context.addPath(path)
            context.fillPath()
            if current {
                context.setStrokeColor(palette.findCurrentOutline)
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

    /// The lines at the right edges `xs`, in one fill (task 2.0a).
    static func drawColumnSeparators(
        atX xs: some Sequence<CGFloat>,
        minY: CGFloat,
        maxY: CGFloat,
        palette: GridPalette,
        context: CGContext,
        scratch: inout [CGRect]
    ) {
        context.setFillColor(palette.gridLine)
        scratch.removeAll(keepingCapacity: true)
        var last: CGFloat?
        for x in xs {
            // Two columns' lines in one place (a column of no width) are
            // filled one over the other, as separate calls draw them: one
            // fill of overlapping rectangles would cover them once.
            if let last, x - 1 < last {
                context.fill(scratch)
                scratch.removeAll(keepingCapacity: true)
            }
            scratch.append(CGRect(x: x - 1, y: minY, width: 1, height: maxY - minY))
            last = x
        }
        context.fill(scratch)
    }

    /// A short ragged row's missing cell (ADR-0002 question 5, mockup 03a):
    /// thin diagonal lines. Only ragged rows have these, so the colour is
    /// resolved here rather than in the palette every frame.
    static func drawHatch(in rect: CGRect, palette: GridPalette, context: CGContext) {
        context.saveGState()
        context.clip(to: rect.insetBy(dx: 0, dy: 1))
        context.setStrokeColor(palette.hatch)
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

    /// An edited, unsaved cell's mark (mockup 05a, task 2.5.2): a small
    /// triangle in the accent colour in the cell's top leading corner.
    static func drawEditedMark(in rect: CGRect, palette: GridPalette, context: CGContext) {
        let size: CGFloat = 6
        let x = rect.minX + 1
        let y = rect.minY + 1
        context.setFillColor(palette.accent)
        context.beginPath()
        context.move(to: CGPoint(x: x, y: y))
        context.addLine(to: CGPoint(x: x + size, y: y))
        context.addLine(to: CGPoint(x: x, y: y + size))
        context.closePath()
        context.fillPath()
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
