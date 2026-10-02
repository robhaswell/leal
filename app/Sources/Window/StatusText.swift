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
    /// Where the bytes are held: in memory, a copy, still being read from a
    /// removable drive, or disconnected from it, each with a note (DESIGN
    /// §3.1, ADR-0006).
    var storage: SourceStorage = .clone
    /// The file changed on its drive while it was read (1.1a).
    var changedOnDisk = false
    /// The index stopped on a read error: the rows shown are all Leal could
    /// read (phase 1 review, app-8).
    var readStopped = false
    /// What has happened to the user's file since it was opened (task
    /// 1.9): changed or deleted elsewhere, or its drive not connected.
    var original: OriginalState = .unchanged
    /// The info-level kinds found, for their notes (ADR-0002 question 7).
    var infoKinds: [DiagnosticKind] = []
    /// How many kinds of warning or error: the badge (mockup 03a).
    var warningKinds = 0
    /// Attributes that were ignored (ADR-0005 decision 5).
    var notes: [InterpretationNote] = []
    /// What **Reopen with encoding** offers.
    var encodingChoices: [TextEncoding] = []
}

/// One of the status bar's segments.
struct StatusItem: Equatable, Sendable {
    enum Role: Equatable, Sendable {
        case plain
        /// The delimiter: the **Treat as** menu.
        case delimiter
        /// The encoding: the **Reopen with encoding** menu.
        case encoding
        /// An info-level kind's note: opens the details popover (the
        /// banner and badge are only for warnings and errors).
        case diagnostics
    }

    let text: String
    var role: Role = .plain
}

/// The status bar's words: `1,000,000 rows × 12 columns · Comma · CRLF ·
/// UTF-8 (BOM)` (mockup 01a), from the String Catalog (DESIGN §4.4). Pure,
/// so it is tested on its own.
enum StatusText {
    /// The segments, left to right, joined with " · " in the bar.
    static func segments(_ status: StatusSummary) -> [String] {
        items(status).map(\.text)
    }

    /// The segments with what each is: the delimiter and the encoding are
    /// menus (ADR-0005 decision 8).
    static func items(_ status: StatusSummary) -> [StatusItem] {
        var items = [StatusItem(text: counts(status))]
        items.append(StatusItem(text: delimiter(status.delimiter), role: .delimiter))
        if let ending = status.lineEnding {
            items.append(StatusItem(text: lineEnding(ending)))
        }
        items.append(StatusItem(text: encoding(status.encoding, source: status.encodingSource), role: .encoding))
        if status.readOnly {
            items.append(StatusItem(text: String(localized: "Read-only", comment: "Status bar: the file can't be edited (UTF-16, DESIGN §4.3)")))
        }
        if let note = storageNote(status) {
            items.append(StatusItem(text: note))
        }
        if let note = originalNote(status) {
            items.append(StatusItem(text: note))
        }
        if let note = notesSegment(status.notes) {
            items.append(StatusItem(text: note))
        }
        for kind in status.infoKinds {
            if let text = DiagnosticsText.info(kind) {
                items.append(StatusItem(text: text, role: .diagnostics))
            }
        }
        if !status.header, status.headerSource == .guess {
            items.append(StatusItem(text: String(localized: "No header row detected", comment: "Status bar: detection found no header row (mockup 06b)")))
        }
        return items
    }

    /// Where the file's bytes are, when it isn't the usual snapshot
    /// (DESIGN §3.1, ADR-0006).
    static func storageNote(_ status: StatusSummary) -> String? {
        if status.changedOnDisk {
            return String(localized: "Changed while reading", comment: "Status bar: the file changed on its drive while Leal read it (1.1a)")
        }
        if status.readStopped {
            return String(localized: "Partly read", comment: "Status bar: a read error stopped Leal before the end of the file; it shows the rows it read")
        }
        return switch status.storage {
        case .clone: nil
        case .memory:
            String(
                localized: "Read into memory",
                comment: "Status bar: the file's volume can't make a snapshot, so Leal read the file into memory (DESIGN §3.1)"
            )
        case .copy:
            String(localized: "Working from a copy", comment: "Status bar: Leal reads its own copy of the file on this Mac (DESIGN §3.1, ADR-0006)")
        case .reading:
            String(localized: "Reading from the drive", comment: "Status bar: the file is on a removable drive, and Leal is still copying it (ADR-0006)")
        case .disconnected:
            String(localized: "Drive disconnected", comment: "Status bar: the file's removable drive was disconnected (ADR-0006)")
        }
    }

    /// What has happened to the file elsewhere (task 1.9), which stays in
    /// the bar after **Keep Editing** hides the banner. Nothing when the
    /// storage note already says it: a change while reading, or a drive
    /// disconnected before the copy was complete.
    static func originalNote(_ status: StatusSummary) -> String? {
        if status.changedOnDisk { return nil }
        return switch status.original {
        case .unchanged: nil
        case .changed:
            String(localized: "Changed on disk", comment: "Status bar: another app changed the open file; Leal shows the version it opened (task 1.9)")
        case .deleted:
            String(localized: "Deleted", comment: "Status bar: the open file was deleted or moved to the Trash (task 1.9)")
        case .unavailable:
            status.storage == .disconnected
                ? nil
                : String(localized: "Drive not connected", comment: "Status bar: the open file's drive was ejected after Leal had copied the file (task 1.9)")
        }
    }

    /// The note for attributes that were ignored (ADR-0005 decision 5).
    static func notesSegment(_ notes: [InterpretationNote]) -> String? {
        let encoding = notes.contains {
            switch $0 {
            case .textEncodingUnreadable, .textEncodingUnsupported, .textEncodingUtf16WithoutBom, .textEncodingDoesNotDecode: true
            case .interpretationUnreadable, .interpretationNotSensible: false
            }
        }
        if encoding {
            return String(localized: "Encoding attribute ignored", comment: "Status bar: the file's com.apple.TextEncoding attribute was ignored (ADR-0005 decision 5)")
        }
        if !notes.isEmpty {
            return String(localized: "Remembered settings ignored", comment: "Status bar: Leal's own remembered delimiter and header choice were ignored (ADR-0005 decision 1)")
        }
        return nil
    }

    /// Why each attribute was ignored, for the tooltip.
    static func noteHelp(_ note: InterpretationNote) -> String {
        switch note {
        case .textEncodingUnreadable:
            String(localized: "The file’s encoding attribute couldn’t be read, so Leal ignored it.", comment: "Status bar tooltip: an ignored attribute")
        case let .textEncodingUnsupported(number):
            String(
                localized: "The file’s encoding attribute names an encoding Leal doesn’t read (number \(String(number))), so Leal ignored it.",
                comment: "Status bar tooltip: an ignored attribute; its CFStringEncoding number"
            )
        case .textEncodingUtf16WithoutBom:
            String(
                localized: "The file’s encoding attribute says UTF-16, but the file has no UTF-16 byte order mark, so Leal ignored it.",
                comment: "Status bar tooltip: an ignored attribute"
            )
        case let .textEncodingDoesNotDecode(encoding):
            String(
                localized: "The file’s encoding attribute says \(encodingName(encoding)), but some of its bytes aren’t valid \(encodingName(encoding)), so Leal ignored it.",
                comment: "Status bar tooltip: an ignored attribute; the encoding it names, twice"
            )
        case .interpretationUnreadable:
            String(localized: "Leal’s remembered settings for this file couldn’t be read, so Leal ignored them.", comment: "Status bar tooltip: an ignored attribute")
        case let .interpretationNotSensible(delimiter):
            String(
                localized: "The file changed since Leal remembered its delimiter (\(Self.delimiter(delimiter))), so Leal guessed again.",
                comment: "Status bar tooltip: Leal's remembered delimiter no longer fits; its name"
            )
        }
    }

    // MARK: Menus (ADR-0005 decision 8)

    static var treatAs: String {
        String(localized: "Treat As", comment: "Menu: read the file with another delimiter (ADR-0005 decision 8)")
    }

    static var reopenWithEncoding: String {
        String(localized: "Reopen with Encoding", comment: "Menu: read the file in another encoding (ADR-0005 decision 8)")
    }

    /// VoiceOver's name for the delimiter menu button.
    static func delimiterMenuLabel(_ delimiter: String) -> String {
        String(localized: "Delimiter: \(delimiter), menu", comment: "VoiceOver: the status bar's Treat As button; the delimiter's name")
    }

    /// VoiceOver's name for the encoding menu button.
    static func encodingMenuLabel(_ encoding: String) -> String {
        String(localized: "Encoding: \(encoding), menu", comment: "VoiceOver: the status bar's Reopen with Encoding button; the encoding as the bar shows it")
    }

    static var treatAsHelp: String {
        String(localized: "Treat the file as separated by another delimiter", comment: "Status bar tooltip of the delimiter menu")
    }

    static var reopenHelp: String {
        String(localized: "Reopen the file with another encoding", comment: "Status bar tooltip of the encoding menu")
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

    /// The status bar's tooltip: where the encoding came from, and why the
    /// file is in memory if it is.
    static func help(_ status: StatusSummary) -> String {
        var lines = [encodingHelp(status.encodingSource)]
        if status.changedOnDisk {
            lines.append(String(
                localized: "The file changed on its drive while Leal was reading it, so Leal shows only what it had read before the change.",
                comment: "Status bar tooltip: the file changed while Leal read it (1.1a, task 1.9)"
            ))
        } else if status.readStopped {
            lines.append(String(
                localized: "Leal couldn’t read the rest of the file, so it shows the rows it had read. File ▸ Reload from Disk tries again.",
                comment: "Status bar tooltip: a read error stopped Leal before the end of the file"
            ))
        } else {
            switch status.storage {
            case .clone:
                break
            case .memory:
                lines.append(String(
                    localized: "This volume can’t make a snapshot of the file, so Leal read the whole file into memory.",
                    comment: "Status bar tooltip: why the file was read into memory (DESIGN §3.1)"
                ))
            case .copy:
                lines.append(String(
                    localized: "Leal copied the file to this Mac and reads the copy, so the file’s drive or volume can go away safely.",
                    comment: "Status bar tooltip: Leal reads its own copy (DESIGN §3.1, ADR-0006)"
                ))
            case .reading:
                lines.append(String(
                    localized: "The file is on a removable drive. Leal reads it from the drive while it copies it to this Mac.",
                    comment: "Status bar tooltip: the copy off a removable drive isn't finished (ADR-0006)"
                ))
            case .disconnected:
                lines.append(String(
                    localized: "The drive was disconnected before Leal had read the whole file. Save is off until it’s back; Save As keeps a copy.",
                    comment: "Status bar tooltip: the removable drive was disconnected (ADR-0006)"
                ))
            }
        }
        if let help = originalHelp(status) {
            lines.append(help)
        }
        lines += status.notes.map(noteHelp)
        return lines.joined(separator: "\n")
    }

    /// The tooltip line for `originalNote`.
    static func originalHelp(_ status: StatusSummary) -> String? {
        guard originalNote(status) != nil else { return nil }
        return switch status.original {
        case .unchanged: nil
        case .changed:
            String(
                localized: "Another app changed this file after Leal opened it. Leal shows the version it opened; File ▸ Reload from Disk shows the new one.",
                comment: "Status bar tooltip: the file changed on disk (task 1.9)"
            )
        case .deleted:
            String(
                localized: "This file was deleted or moved to the Trash after Leal opened it. Leal shows the version it opened.",
                comment: "Status bar tooltip: the file was deleted (task 1.9)"
            )
        case .unavailable:
            String(
                localized: "The drive with this file isn’t connected. Leal has the whole file; Save is off until the drive is back.",
                comment: "Status bar tooltip: the file's drive was ejected after Leal had copied the file (task 1.9)"
            )
        }
    }

    /// "31%".
    static func percent(_ fraction: Double) -> String {
        min(1, max(0, fraction)).formatted(.percent.precision(.fractionLength(0)))
    }
}
