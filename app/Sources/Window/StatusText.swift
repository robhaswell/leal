import Foundation
import LealFFI

/// What the status bar shows (DESIGN §4.1), all of it from the core.
struct StatusSummary: Equatable, Sendable {
    /// Data rows (the header row isn't one): the estimate while indexing.
    var rows: Int
    /// The most common field count.
    var columns: Int
    var indexing: Bool
    /// How far indexing has got, 0 to 1.
    var fractionIndexed: Double
    var delimiter: Delimiter
    var lineEnding: LineEnding?
    var encoding: TextEncoding
    var encodingSource: EncodingSource
    var header: Bool
    var headerSource: DialectSource
    var readOnly: Bool
}

/// The status bar's words: `1,000,000 rows × 12 columns · Comma · CRLF ·
/// UTF-8 (BOM)` (mockup 01a), from the String Catalog (DESIGN §4.4). Pure,
/// so it is tested on its own.
enum StatusText {
    /// The segments, left to right, joined with " · " in the bar.
    static func segments(_ status: StatusSummary) -> [String] {
        var segments = [counts(status)]
        segments.append(delimiter(status.delimiter))
        if let ending = status.lineEnding {
            segments.append(lineEnding(ending))
        }
        segments.append(encoding(status.encoding, source: status.encodingSource))
        if status.readOnly {
            segments.append(String(localized: "Read-only", comment: "Status bar: the file can't be edited (UTF-16, DESIGN §4.3)"))
        }
        if !status.header, status.headerSource == .guess {
            segments.append(String(localized: "No header row detected", comment: "Status bar: detection found no header row (mockup 06b)"))
        }
        return segments
    }

    /// "1,000,000 rows × 12 columns", or "Indexing… about 9,400,000 rows ×
    /// 12 columns" while the count is an estimate (mockup 02a).
    static func counts(_ status: StatusSummary) -> String {
        let rows = status.rows == 1
            ? String(localized: "1 row", comment: "Status bar: one data row")
            : String(localized: "\(status.rows.formatted()) rows", comment: "Status bar: the number of data rows")
        let columns = status.columns == 1
            ? String(localized: "1 column", comment: "Status bar: one column")
            : String(localized: "\(status.columns.formatted()) columns", comment: "Status bar: the number of columns")
        return status.indexing
            ? String(localized: "Indexing… about \(rows) × \(columns)", comment: "Status bar while indexing: the estimated rows, then the columns")
            : String(localized: "\(rows) × \(columns)", comment: "Status bar: rows, then columns, as in 1,000 rows × 12 columns")
    }

    static func delimiter(_ delimiter: Delimiter) -> String {
        switch delimiter {
        case .comma: String(localized: "Comma", comment: "Status bar: the delimiter")
        case .semicolon: String(localized: "Semicolon", comment: "Status bar: the delimiter")
        case .tab: String(localized: "Tab", comment: "Status bar: the delimiter")
        case .pipe: String(localized: "Pipe", comment: "Status bar: the delimiter")
        }
    }

    /// Line endings by their usual names, which aren't translated.
    static func lineEnding(_ ending: LineEnding) -> String {
        switch ending {
        case .lf: "LF"
        case .crlf: "CRLF"
        case .cr: "CR"
        }
    }

    /// The encoding's name, and where it came from (ADR-0005 decision 8):
    /// "UTF-8 (BOM)", "Windows-1252 (file attribute)", "UTF-8 (chosen)". A
    /// guess shows the name alone, as the mockups draw it; the tooltip
    /// (`encodingHelp`) says it was guessed.
    static func encoding(_ encoding: TextEncoding, source: EncodingSource) -> String {
        let name = encodingName(encoding)
        return switch source {
        case .bom: String(localized: "\(name) (BOM)", comment: "Status bar: the encoding, from the file's byte order mark")
        case .attribute: String(localized: "\(name) (file attribute)", comment: "Status bar: the encoding, from the file's com.apple.TextEncoding attribute")
        case .user: String(localized: "\(name) (chosen)", comment: "Status bar: the encoding the user chose with Reopen with Encoding")
        case .guess: name
        }
    }

    /// The status bar's tooltip for where the encoding came from.
    static func encodingHelp(_ source: EncodingSource) -> String {
        switch source {
        case .bom: String(localized: "Leal read the encoding from the file’s byte order mark.", comment: "Status bar tooltip: encoding source")
        case .attribute: String(localized: "Leal read the encoding from the file’s encoding attribute, which the app that saved it set.", comment: "Status bar tooltip: encoding source")
        case .user: String(localized: "You chose this encoding.", comment: "Status bar tooltip: encoding source")
        case .guess: String(localized: "Leal guessed the encoding from the start of the file.", comment: "Status bar tooltip: encoding source")
        }
    }

    /// Encoding names as macOS and the web write them; not translated.
    static func encodingName(_ encoding: TextEncoding) -> String {
        switch encoding {
        case .utf8: "UTF-8"
        case .utf16Le: "UTF-16 LE"
        case .utf16Be: "UTF-16 BE"
        case .windows1250: "Windows-1250"
        case .windows1251: "Windows-1251"
        case .windows1252: "Windows-1252"
        case .windows1253: "Windows-1253"
        case .windows1254: "Windows-1254"
        case .windows1255: "Windows-1255"
        case .windows1256: "Windows-1256"
        case .windows1257: "Windows-1257"
        case .windows1258: "Windows-1258"
        case .iso88591: "ISO-8859-1"
        case .iso88592: "ISO-8859-2"
        case .iso885915: "ISO-8859-15"
        case .macRoman: "Mac Roman"
        }
    }

    /// "31%".
    static func percent(_ fraction: Double) -> String {
        min(1, max(0, fraction)).formatted(.percent.precision(.fractionLength(0)))
    }
}
