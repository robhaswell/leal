import Foundation
import LealFFI

/// The words of the diagnostics banner, the details popover and the other
/// banners of task 1.7 (DESIGN §3.5, ADR-0002 question 7, ADR-0005
/// decision 8, ADR-0006), from the String Catalog (DESIGN §4.4). Pure, so
/// it is tested on its own. Every number in it comes from the core.
enum DiagnosticsText {
    // MARK: The banner (mockup 03a)

    /// "This file has 3 kinds of irregularity. It’s shown exactly as
    /// written." (ADR-0002 question 7).
    static func banner(kinds: Int) -> String {
        kinds == 1
            ? String(
                localized: "This file has 1 kind of irregularity. It’s shown exactly as written.",
                comment: "Diagnostics banner (DESIGN §3.5, mockup 03a) with one kind of warning or error"
            )
            : String(
                localized: "This file has \(kinds.formatted()) kinds of irregularity. It’s shown exactly as written.",
                comment: "Diagnostics banner (DESIGN §3.5, mockup 03a); the number of kinds of warning or error"
            )
    }

    static var details: String {
        String(localized: "Details", comment: "Diagnostics banner button: shows the details popover (mockup 03a)")
    }

    // MARK: The details popover (mockup 03b)

    static func popoverTitle(fileName: String) -> String {
        String(localized: "Irregularities in \(fileName)", comment: "Title of the diagnostics details popover (mockup 03b); the file's name")
    }

    /// A kind's name: "Ragged rows", "Invalid UTF-8".
    static func title(_ kind: DiagnosticKind, encoding: TextEncoding) -> String {
        switch kind {
        case .unterminatedQuote:
            String(localized: "Unterminated quote", comment: "Diagnostic kind (DESIGN §3.5)")
        case .raggedRows:
            String(localized: "Ragged rows", comment: "Diagnostic kind: rows with a different number of fields (DESIGN §3.5)")
        case .textAfterClosingQuote:
            String(localized: "Text after closing quote", comment: "Diagnostic kind: text such as \"a\"b (DESIGN §3.5)")
        case .invalidEncoding:
            String(
                localized: "Invalid \(StatusText.encodingName(encoding))",
                comment: "Diagnostic kind: bytes that aren't valid in the file's encoding, as in Invalid UTF-8 (mockup 03b)"
            )
        case .nulBytes:
            String(localized: "NUL bytes", comment: "Diagnostic kind (DESIGN §3.5)")
        case .mixedLineEndings:
            String(localized: "Mixed line endings", comment: "Diagnostic kind: some rows end with LF, some with CRLF (DESIGN §3.5)")
        case .blankLines:
            String(localized: "Blank lines", comment: "Diagnostic kind: empty rows (DESIGN §3.5)")
        case .bomPresent:
            String(localized: "Byte order mark", comment: "Diagnostic kind: the file starts with a BOM (DESIGN §3.5)")
        }
    }

    /// Where a physical row is, in the gutter's numbers: "row 17", or "the
    /// header row".
    static func row(_ physical: UInt64, headerRows: Int) -> String {
        if physical < UInt64(headerRows) {
            return String(localized: "the header row", comment: "Where a diagnostic is: in the file's header row")
        }
        let number = Int(physical) - headerRows + 1
        return String(localized: "row \(number.formatted())", comment: "Where a diagnostic is: a row number as the gutter shows it")
    }

    /// The line under a kind's name: what and where (mockup 03b).
    static func subtitle(
        _ diagnostic: Diagnostic,
        rowsWithCommonFieldCount: UInt64,
        headerRows: Int,
        dominantLineEnding: LineEnding?,
        encoding: TextEncoding
    ) -> String {
        let count = Int(diagnostic.count)
        let first = diagnostic.first.first.map { row($0.row, headerRows: headerRows) } ?? ""
        let others = Int(rowsWithCommonFieldCount).formatted()
        switch diagnostic.kind {
        case .unterminatedQuote:
            return String(
                localized: "A quote at \(first) is never closed, so the rest of the file is one cell",
                comment: "Details popover: the unterminated quote; where its opening quote is, such as row 12"
            )
        case .raggedRows:
            return count == 1
                ? String(
                    localized: "1 row has a different number of fields to the other \(others)",
                    comment: "Details popover: one ragged row; the number of rows with the usual field count"
                )
                : String(
                    localized: "\(count.formatted()) rows have a different number of fields to the other \(others)",
                    comment: "Details popover: ragged rows (mockup 03b); their count, then the number of rows with the usual field count"
                )
        case .textAfterClosingQuote, .nulBytes:
            return count == 1
                ? String(localized: "1 cell, at \(first)", comment: "Details popover: one cell has this kind (mockup 03b); where, such as row 17")
                : String(
                    localized: "\(count.formatted()) cells, the first at \(first)",
                    comment: "Details popover: cells with this kind; their count, and where the first is"
                )
        case .invalidEncoding:
            return count == 1
                ? String(
                    localized: "1 cell, at \(first) — shown as �",
                    comment: "Details popover: one cell with bytes that don't decode (mockup 03b); where, such as row 11"
                )
                : String(
                    localized: "\(count.formatted()) cells, the first at \(first) — shown as �",
                    comment: "Details popover: cells with bytes that don't decode; their count, and where the first is"
                )
        case .mixedLineEndings:
            if let ending = dominantLineEnding {
                let name = StatusText.lineEnding(ending)
                return count == 1
                    ? String(
                        localized: "1 row ends differently; the rest end with \(name)",
                        comment: "Details popover: one row has another line ending; the most common one, such as CRLF"
                    )
                    : String(
                        localized: "\(count.formatted()) rows end differently; the rest end with \(name)",
                        comment: "Details popover: rows with another line ending (mockup 03b); their count, then the most common one"
                    )
            }
            return String(localized: "\(count.formatted()) rows end differently from the rest", comment: "Details popover: rows with another line ending; their count")
        case .blankLines:
            return count == 1
                ? String(localized: "1 empty row, at \(first)", comment: "Details popover: one blank line; where")
                : String(
                    localized: "\(count.formatted()) empty rows, the first at \(first)",
                    comment: "Details popover: blank lines; their count, and where the first is"
                )
        case .bomPresent:
            return String(
                localized: "The file starts with a \(StatusText.encodingName(encoding)) byte order mark",
                comment: "Details popover: the file has a BOM; its encoding, such as UTF-8"
            )
        }
    }

    /// "1 of 3", or past the report's first locations "1,000+ of 3,000".
    static func position(_ position: KindPosition, count: UInt64) -> String {
        let total = Int(count).formatted()
        switch position {
        case let .at(index):
            return String(localized: "\(index.formatted()) of \(total)", comment: "Details popover: which occurrence is shown, as in 1 of 3 (mockup 03b)")
        case let .beyond(known):
            return String(
                localized: "\(known.formatted())+ of \(total)",
                comment: "Details popover: an occurrence past the first ones Leal lists, as in 1,000+ of 3,000"
            )
        }
    }

    /// The Previous arrow, naming its kind: "Previous: Ragged rows".
    static func previous(_ title: String) -> String {
        String(localized: "Previous: \(title)", comment: "Details popover: go to the previous occurrence of a kind (VoiceOver and tooltip); the kind's name")
    }

    /// The Next arrow: "Next: Ragged rows".
    static func next(_ title: String) -> String {
        String(localized: "Next: \(title)", comment: "Details popover: go to the next occurrence of a kind (VoiceOver and tooltip); the kind's name")
    }

    static var showDetailsHelp: String {
        String(localized: "Show the file’s irregularities", comment: "Tooltip of a status bar note that opens the diagnostics details")
    }

    // MARK: The status bar

    /// The status bar's badge: how many kinds of warning or error.
    static func badgeHelp(kinds: Int) -> String {
        String(localized: "Show the irregularities (\(kinds.formatted()) kinds)", comment: "Tooltip of the status bar's diagnostics badge")
    }

    /// Info-level kinds, which appear only in the status bar and the
    /// details (ADR-0002 question 7). The BOM is already in the encoding
    /// segment, "UTF-8 (BOM)".
    static func info(_ kind: DiagnosticKind) -> String? {
        switch kind {
        case .mixedLineEndings:
            String(localized: "Mixed line endings", comment: "Status bar: some rows end with another line ending (mockup 03a)")
        case .blankLines:
            String(localized: "Blank lines", comment: "Status bar: the file has empty rows")
        default:
            nil
        }
    }

    // MARK: Suggestions (DESIGN §3.2, ADR-0005 decision 4)

    /// "This file looks semicolon-separated."
    static func delimiterSuggestion(_ delimiter: Delimiter) -> String {
        switch delimiter {
        case .comma:
            String(localized: "This file looks comma-separated.", comment: "Suggestion banner: the whole file fits another delimiter (DESIGN §3.2)")
        case .semicolon:
            String(localized: "This file looks semicolon-separated.", comment: "Suggestion banner: the whole file fits another delimiter (DESIGN §3.2)")
        case .tab:
            String(localized: "This file looks tab-separated.", comment: "Suggestion banner: the whole file fits another delimiter (DESIGN §3.2)")
        case .pipe:
            String(localized: "This file looks pipe-separated.", comment: "Suggestion banner: the whole file fits another delimiter (DESIGN §3.2)")
        }
    }

    static var switchDelimiter: String {
        String(localized: "Switch", comment: "Suggestion banner button: read the file with the suggested delimiter (DESIGN §3.2)")
    }

    /// "This file looks like Windows-1252."
    static func encodingSuggestion(_ encoding: TextEncoding) -> String {
        String(
            localized: "This file looks like \(StatusText.encodingName(encoding)).",
            comment: "Suggestion banner: the whole file fits another encoding (ADR-0005 decision 4); its name"
        )
    }

    /// "Reopen as Windows-1252".
    static func reopenAs(_ encoding: TextEncoding) -> String {
        String(
            localized: "Reopen as \(StatusText.encodingName(encoding))",
            comment: "Suggestion banner button: read the file in the suggested encoding (ADR-0005 decision 4)"
        )
    }

    // MARK: Removable drives (ADR-0006)

    static var disconnected: String {
        String(
            localized: "The drive with this file was disconnected. Leal shows the rows it had read; Save is off until the drive is back, and Save As keeps a copy.",
            comment: "Banner: the file's removable drive was disconnected before Leal had read it all (ADR-0006)"
        )
    }

    static var changedWhileReading: String {
        String(
            localized: "This file changed on its drive while Leal was reading it, so what’s shown may mix two versions. Save is off; Save As keeps a copy.",
            comment: "Banner: the file changed while Leal read it from a drive that can't make a snapshot (1.1a)"
        )
    }

    static var saveAs: String {
        String(localized: "Save As…", comment: "Banner button: save a copy of the file elsewhere (ADR-0006)")
    }
}

/// Which occurrence of a kind the details popover is on.
enum KindPosition: Equatable, Sendable {
    /// The `index`th (from 1), as the report's locations show.
    case at(Int)
    /// Past the report's `known` first locations: the core found it from
    /// the row marks.
    case beyond(known: Int)
}

/// Where the details popover's **Previous** and **Next** are for each kind
/// (mockup 03b). Pure: the rows come from the core's report and its
/// `nextWithKind`/`previousWithKind`, which find occurrences past the
/// report's first 1,000 too.
///
/// A kind starts on its first occurrence, unvisited ("1 of 3"): **Next**
/// then shows that first occurrence, and after that the next one.
struct KindNavigation: Equatable, Sendable {
    /// The physical row each visited kind is on.
    private(set) var rows: [DiagnosticKind: UInt64] = [:]
    /// The kind navigated last, which the popover highlights.
    private(set) var current: DiagnosticKind?

    /// Kinds whose Next found nothing more: Next stays off (it doesn't wrap).
    private(set) var ended: Set<DiagnosticKind> = []

    mutating func visit(_ kind: DiagnosticKind, row: UInt64) {
        if let old = rows[kind], row < old { ended.remove(kind) }
        rows[kind] = row
        current = kind
    }

    /// Next from where `kind` is found nothing: it is on the last one.
    mutating func reachedEnd(_ kind: DiagnosticKind) {
        ended.insert(kind)
    }

    mutating func reset() {
        rows = [:]
        ended = []
        current = nil
    }

    func row(of kind: DiagnosticKind) -> UInt64? {
        rows[kind]
    }

    /// Where **Next** searches from: the first row, or the row after the
    /// current one.
    func nextStart(_ kind: DiagnosticKind) -> UInt64 {
        rows[kind].map { $0 + 1 } ?? 0
    }

    /// Where **Previous** searches before, or `nil` if it can't go back.
    func previousEnd(_ kind: DiagnosticKind) -> UInt64? {
        rows[kind]
    }

    /// Which occurrence `kind` is on: from the report's first locations,
    /// or past them.
    func position(of diagnostic: Diagnostic) -> KindPosition {
        guard let row = rows[diagnostic.kind] else { return .at(1) }
        let known = diagnostic.first.count
        // The first location in this row, counting from 1.
        if let index = diagnostic.first.firstIndex(where: { $0.row >= row }), diagnostic.first[index].row == row {
            return .at(index + 1)
        }
        if UInt64(known) >= diagnostic.count {
            // Every occurrence is listed, so the row must be among them;
            // count the ones before it.
            return .at(max(1, diagnostic.first.filter { $0.row < row }.count))
        }
        return .beyond(known: known)
    }

    /// Whether **Previous** can move: the kind has been visited, and isn't
    /// on its first occurrence.
    func canGoBack(_ diagnostic: Diagnostic) -> Bool {
        guard rows[diagnostic.kind] != nil else { return false }
        if case .at(1) = position(of: diagnostic) { return false }
        return true
    }

    /// Whether **Next** can move: unvisited, or not on the last occurrence.
    func canGoForward(_ diagnostic: Diagnostic) -> Bool {
        if ended.contains(diagnostic.kind) { return false }
        guard diagnostic.count > 0, let row = rows[diagnostic.kind] else { return diagnostic.count > 0 }
        if UInt64(diagnostic.first.count) >= diagnostic.count, let last = diagnostic.first.last {
            return row < last.row
        }
        return true
    }
}
