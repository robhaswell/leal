import AppKit

// Synthetic data source and the per-cell styling rules shared by both grids.
//
// Cell text is generated on demand from a hash of (row, column), with no big
// arrays, to mimic asking the Rust core for a row. Both grid implementations
// call exactly the same functions, so any cost here is paid equally.

struct Config {
    var impl = "custom"          // "table" (A) or "custom" (B)
    var cols = 12
    var rows = 1_000_000
    var bench = false            // run the scripted scroll and quit
    var out: String?             // JSON results path
    var snapshot: String?        // save a PNG of the window and quit
    var snapshotEnd: String?     // same, after jumping to the last row
    var sampleRows = 1000        // column auto-size sample (ADR-0002 q2)
    var styling = true           // per-cell styling on (ADR-0002)
    var editor = true            // open one in-cell editor before measuring
    var flingRows = 50_000       // rows covered by the vertical flings
    var hold = false             // stay open after the bench (for `footprint`)
    var screen: Int?             // NSScreen.screens index; default: fastest refresh
    var flatten = false          // A only: rows draw their cell views into one layer
    var lite = false             // A only: cells draw their own text (no NSTextField)
    var speed = 60_000.0         // initial fling velocity, pt/s
    var dark = false             // system appearance instead of forcing light

    static func parse() -> Config {
        var c = Config()
        var args = CommandLine.arguments.dropFirst().makeIterator()
        while let a = args.next() {
            switch a {
            case "--impl": c.impl = args.next() ?? c.impl
            case "--cols": c.cols = Int(args.next() ?? "") ?? c.cols
            case "--rows": c.rows = Int(args.next() ?? "") ?? c.rows
            case "--bench": c.bench = true
            case "--out": c.out = args.next()
            case "--snapshot": c.snapshot = args.next()
            case "--snapshot-end": c.snapshotEnd = args.next()
            case "--sample-rows": c.sampleRows = Int(args.next() ?? "") ?? c.sampleRows
            case "--no-styling": c.styling = false
            case "--no-editor": c.editor = false
            case "--fling-rows": c.flingRows = Int(args.next() ?? "") ?? c.flingRows
            case "--hold": c.hold = true
            case "--dark": c.dark = true
            case "--flatten": c.flatten = true
            case "--lite": c.lite = true
            case "--speed": c.speed = Double(args.next() ?? "") ?? c.speed
            case "--screen": c.screen = Int(args.next() ?? "")
            default: break  // ignore -NSDocumentRevisionsDebugMode etc.
            }
        }
        precondition(c.impl == "table" || c.impl == "custom", "--impl table|custom")
        return c
    }
}

@inline(__always) func mix(_ x: UInt64) -> UInt64 {
    // splitmix64 finaliser: cheap, deterministic, well distributed.
    var z = x &+ 0x9E37_79B9_7F4A_7C15
    z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
    z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
    return z ^ (z >> 31)
}

enum Kind: Int, CaseIterable {
    case id, date, customer, email, country, sku, product, qty, price, total, currency, notes
    var header: String {
        ["order_id", "order_date", "customer", "email", "country", "sku", "product",
         "qty", "unit_price", "total", "currency", "notes"][rawValue]
    }
    var numeric: Bool { self == .qty || self == .price || self == .total }
}

private let customers = [
    "Sable Optics", "Loire Provisions", "Verity Labs", "Marlow Foods", "Tamsin Knits",
    "Halden & Co", "Ostrava Tools", "Pinecrest Supply", "Redfern Bikes", "Marlow & Daughters",
    "Ullswater Pumps", "Quill Stationery", "Wexford Paper", "Brightwater Ltd", "Abbot Fasteners",
    "Cordell Glass", "Dunmore Textiles", "Ilford Adhesives", "Kelso Print", "Selkirk Brushes",
]
private let slugs = customers.map { name -> String in
    name.lowercased().filter { $0.isLetter }.prefix(14).description
}
private let countries = ["GB", "FR", "US", "DE", "NL", "IE"]
private let currencyFor = ["GB": "GBP", "FR": "EUR", "US": "USD", "DE": "EUR", "NL": "EUR", "IE": "EUR"]
private let products: [(sku: String, name: String, cents: Int)] = [
    ("BK-7003", "Bookends, cast iron", 2990), ("KT-8021", "Kitchen twine, 100 m", 410),
    ("LT-1043", "Linen tote, indigo", 1850), ("LT-1042", "Linen tote, natural", 1850),
    ("TW-6010", "Tea towel, stripe", 840), ("CD-5120", "Beeswax candle, pair", 1400),
    ("NB-3002", "Notebook A5, ruled", 675), ("NB-3001", "Notebook A5, dotted", 675),
    ("MB-2210", "Enamel mug, 350 ml", 920), ("PN-4100", "Fountain pen, fine nib", 4200),
]
private let qtys = [1, 2, 3, 4, 5, 6, 8, 10, 12, 20, 24, 40]
private let prefixes = Array("ABCDEFGHJKLMNPQRSTUVWXYZ")

@inline(__always) private func money(_ cents: Int) -> String {
    let c = cents % 100
    return "\(cents / 100).\(c < 10 ? "0" : "")\(c)"
}
@inline(__always) private func two(_ n: Int) -> String { n < 10 ? "0\(n)" : "\(n)" }

final class DataModel {
    let rows: Int
    let cols: Int
    let styling: Bool
    let findQuery = "marlow"
    /// Committed in-cell edits (the real app keeps these in the core's overlay).
    var edits: [Int: String] = [:]
    var activeRow = 8
    var activeCol = 2
    let currentFind = (row: 5, col: 2)

    init(rows: Int, cols: Int, styling: Bool) {
        self.rows = rows
        self.cols = cols
        self.styling = styling
    }

    func kind(_ col: Int) -> Kind { Kind(rawValue: col % 12)! }
    func header(_ col: Int) -> String {
        col < 12 ? kind(col).header : "\(kind(col).header)_\(col / 12 + 1)"
    }
    func numeric(_ col: Int) -> Bool { kind(col).numeric }

    @inline(__always) private func rowHash(_ row: Int) -> UInt64 { mix(UInt64(row)) }
    @inline(__always) private func cellHash(_ row: Int, _ col: Int) -> UInt64 {
        mix(UInt64(row) << 16 ^ UInt64(col))
    }

    /// Number of fields in this row. One row in 40 is short (ragged),
    /// missing 1–6 trailing fields; the grid shows those cells hatched.
    func fieldCount(_ row: Int) -> Int {
        guard styling, row % 40 == 3 else { return cols }
        let missing = 1 + Int((rowHash(row) >> 8) % UInt64(min(cols - 1, 6)))
        return cols - missing
    }

    /// Diagnostics marker in the gutter (ragged rows).
    func hasDiagnostic(_ row: Int) -> Bool { styling && row % 40 == 3 }

    /// The display value of a cell, or nil for a missing (ragged) cell.
    func cell(_ row: Int, _ col: Int) -> String? {
        if col >= fieldCount(row) { return nil }
        if !edits.isEmpty, let e = edits[row &* cols &+ col] { return e }
        let group = col / 12
        let g = mix(UInt64(row) << 8 ^ UInt64(group))  // shared by a group's 12 columns
        let cust = Int(g % UInt64(customers.count))
        let prod = products[Int((g >> 8) % UInt64(products.count))]
        let qty = qtys[Int((g >> 16) % UInt64(qtys.count))]
        let country = countries[Int((g >> 24) % UInt64(countries.count))]
        switch kind(col) {
        case .id: return "\(prefixes[group % prefixes.count])-\(100231 + row)"
        case .date:
            let d = row / 2800
            return "2025-\(two(1 + (d / 28) % 12))-\(two(1 + d % 28))"
        case .customer: return customers[cust]
        case .email: return "orders@\(slugs[cust]).example"
        case .country: return country
        case .sku: return prod.sku
        case .product: return prod.name
        case .qty: return "\(qty)"
        case .price: return money(prod.cents)
        case .total: return money(prod.cents * qty)
        case .currency: return currencyFor[country]!
        case .notes:
            let h = cellHash(row, col) % 100
            if h < 70 { return "" }
            if h < 85 { return "Gift wrap" }
            if h < 92 { return "Leave with reception" }
            if h < 97 { return "Invoice to head office" }
            return "Customer asked for the parcel to be left with the neighbour at number 14 if "
                + "nobody is home; call ahead on weekdays after 3pm and quote order \(100231 + row). "
                + "Fragile items, do not stack."
        }
    }

    /// Edited-but-unsaved cell: corner triangle. About 1 cell in 150, plus real commits.
    func isEdited(_ row: Int, _ col: Int) -> Bool {
        guard styling else { return false }
        if !edits.isEmpty, edits[row &* cols &+ col] != nil { return true }
        return cellHash(row, col) % 150 == 7
    }

    /// Find-match range in the cell's text (UTF-16), if any.
    func findRange(_ text: String) -> NSRange? {
        guard styling, !text.isEmpty else { return nil }
        let r = (text as NSString).range(of: findQuery, options: .caseInsensitive)
        return r.location == NSNotFound ? nil : r
    }

    func isActive(_ row: Int, _ col: Int) -> Bool { styling && row == activeRow && col == activeCol }
    func isCurrentFind(_ row: Int, _ col: Int) -> Bool {
        styling && row == currentFind.row && col == currentFind.col
    }
}

/// Column geometry shared by both grids: widths auto-sized from a sample of
/// rows (ADR-0002: first 1,000 rows, 260 px maximum).
struct Columns {
    var widths: [CGFloat]
    var xs: [CGFloat]  // left edge of each column; xs[count] is the total width
    var total: CGFloat { xs.last! }

    static let pad: CGFloat = 8
    static let maxWidth: CGFloat = 260
    static let minWidth: CGFloat = 36

    static func autosize(_ m: DataModel, sampleRows: Int) -> Columns {
        var widths = [CGFloat](repeating: 0, count: m.cols)
        for c in 0..<m.cols {
            widths[c] = Text.width(m.header(c), font: Text.headerFont)
        }
        let n = min(sampleRows, m.rows)
        for r in 0..<n {
            for c in 0..<m.cols {
                guard let s = m.cell(r, c), !s.isEmpty else { continue }
                let w = Text.width(s, font: Text.font)
                if w > widths[c] { widths[c] = w }
            }
        }
        let ws = widths.map { min(maxWidth, max(minWidth, ceil($0) + 2 * pad)) }
        return Columns(widths: ws)
    }

    init(widths: [CGFloat]) {
        self.widths = widths
        var xs = [CGFloat](repeating: 0, count: widths.count + 1)
        for i in 0..<widths.count { xs[i + 1] = xs[i] + widths[i] }
        self.xs = xs
    }

    /// Columns intersecting [minX, maxX).
    func range(_ minX: CGFloat, _ maxX: CGFloat) -> Range<Int> {
        func firstEnd(after x: CGFloat) -> Int {  // first column whose right edge > x
            var lo = 0, hi = widths.count
            while lo < hi {
                let mid = (lo + hi) / 2
                if xs[mid + 1] <= x { lo = mid + 1 } else { hi = mid }
            }
            return lo
        }
        let a = firstEnd(after: minX)
        var b = a
        while b < widths.count && xs[b] < maxX { b += 1 }
        return a..<b
    }
}

enum Metrics {
    static let rowHeight: CGFloat = 22
    static let headerHeight: CGFloat = 28
    static let statusHeight: CGFloat = 24
    static let windowSize = NSSize(width: 1200, height: 780)
}
