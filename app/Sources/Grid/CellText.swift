import AppKit
import CoreText

/// How a cell's text is shown on its one grid line. The core gives values
/// as they are (the 1.4 notes' open question); this is presentation only,
/// and never changes a value:
///
/// - a line break (CR, LF or CRLF) shows as a small `↵` (ADR-0002
///   question 11: "multiline values show on one grid line with a small ↵
///   between lines");
/// - a tab shows as `⇥`;
/// - NUL and the other control characters show as their Unicode control
///   pictures (`␀`, `␁`, …), so they are visible rather than invisible.
///
/// The symbols are drawn in the secondary text colour.
enum CellText {
    /// The text to draw for `value`, and where the symbols are (UTF-16
    /// ranges, for colouring them). Values without control characters
    /// come back unchanged with no symbols, which is almost every value.
    static func display(_ value: String) -> (text: String, symbols: [NSRange]) {
        guard value.utf8.contains(where: { $0 < 0x20 || $0 == 0x7F }) else {
            return (value, [])
        }
        var text = ""
        var symbols: [NSRange] = []
        var utf16 = 0
        var previousWasCR = false
        for scalar in value.unicodeScalars {
            let symbol: Character?
            switch scalar.value {
            case 0x0A where previousWasCR:
                // The LF of a CRLF: one ↵ for the pair.
                previousWasCR = false
                continue
            case 0x0A, 0x0D:
                symbol = "↵"
            case 0x09:
                symbol = "⇥"
            case 0x00..<0x20:
                symbol = Character(Unicode.Scalar(0x2400 + scalar.value) ?? "?")
            case 0x7F:
                symbol = "␡"
            default:
                symbol = nil
            }
            previousWasCR = scalar.value == 0x0D
            if let symbol {
                symbols.append(NSRange(location: utf16, length: 1))
                text.append(symbol)
                utf16 += 1
            } else {
                text.unicodeScalars.append(scalar)
                utf16 += scalar.utf16.count
            }
        }
        return (text, symbols)
    }
}

/// Measures text for column sizing, cheaply: for printable ASCII it adds
/// up the font's advances from a table (no Core Text line per cell), and
/// only other text gets a real `CTLine`. Kerning is ignored, which is fine
/// for sizing.
///
/// It is immutable after `init`, so it can measure on any thread (the
/// 1,000-row sizing runs in the background, DESIGN §3.10 P2).
final class TextMeasurer: @unchecked Sendable {
    // @unchecked: `CTFont` isn't marked Sendable, but Core Text fonts are
    // immutable and documented as safe to use from any thread, and the
    // table is a `let`.
    private let font: CTFont
    private let advances: [CGFloat]

    init(font: NSFont) {
        let font = font as CTFont
        self.font = font
        // Each character laid out by Core Text, not looked up as a glyph:
        // the tabular digits of `monospacedDigitSystemFont` are a font
        // feature that only shaping applies.
        advances = (0x20...0x7E).map { code in
            let attributed = NSAttributedString(string: String(UnicodeScalar(UInt8(code))), attributes: [.font: font])
            return CGFloat(CTLineGetTypographicBounds(CTLineCreateWithAttributedString(attributed), nil, nil, nil))
        }
    }

    /// The width of `text` drawn on one line, in points.
    func width(of text: String) -> CGFloat {
        var total: CGFloat = 0
        for byte in text.utf8 {
            guard byte >= 0x20, byte <= 0x7E else { return lineWidth(text) }
            total += advances[Int(byte) - 0x20]
        }
        return total
    }

    /// The width of a cell's value as the grid draws it.
    func cellWidth(of value: String, truncated: Bool) -> CGFloat {
        let shown = CellText.display(value).text
        return width(of: truncated ? shown + "…" : shown)
    }

    private func lineWidth(_ text: String) -> CGFloat {
        let attributed = NSAttributedString(string: text, attributes: [.font: font])
        let line = CTLineCreateWithAttributedString(attributed)
        return CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
    }
}

/// Column widths from a sample of rows (DESIGN §4.1): each column as wide
/// as its widest cell or header title, plus padding, between
/// `GridMetrics.minimumColumnWidth` and a maximum (260 pt, ADR-0002
/// question 2). Pure, so it is tested with any measure.
enum ColumnSizer {
    /// One width per column in `0..<columns`. `header` holds the titles
    /// (a column without one is measured by its cells alone); `rows` the
    /// sample's cells, as (text, truncated). `measureCell` gives a cell's
    /// width in its column (numbers are drawn in another font), and
    /// `measureHeader` a title's.
    static func widths(
        columns: Int,
        header: [String],
        rows: [[(text: String, truncated: Bool)]],
        maximum: CGFloat = GridMetrics.maximumColumnWidth,
        measureCell: (_ column: Int, _ text: String, _ truncated: Bool) -> CGFloat,
        measureHeader: (String) -> CGFloat
    ) -> [CGFloat] {
        guard columns > 0 else { return [] }
        var widest = [CGFloat](repeating: 0, count: columns)
        for (column, title) in header.prefix(columns).enumerated() {
            widest[column] = measureHeader(title)
        }
        for row in rows {
            for (column, cell) in row.prefix(columns).enumerated() where !cell.text.isEmpty || cell.truncated {
                // Once a column is at the maximum, its other cells can't
                // change anything.
                guard widest[column] + 2 * GridMetrics.cellPadding < maximum else { continue }
                widest[column] = max(widest[column], measureCell(column, cell.text, cell.truncated))
            }
        }
        return widest.map { width in
            let padded = (width + 2 * GridMetrics.cellPadding).rounded(.up)
            return min(maximum, max(GridMetrics.minimumColumnWidth, padded))
        }
    }
}
