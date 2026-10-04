import Foundation
import LealFFI

/// The words of undo, Reload with edits and Recover changes (task 2.5.2).
enum HistoryText {
    static let edited = String(localized: "Edited", comment: "Window title suffix while the document has unsaved edits, after an em dash: “orders.csv — Edited” (mockup 05a)")

    /// The window's title while there are unsaved edits (mockup 05a).
    static func editedTitle(_ name: String) -> String {
        String(localized: "\(name) — \(edited)", comment: "Window title with unsaved edits: the file's name, then “Edited”")
    }

    // MARK: Undo

    static func undoRefused(_ name: String, undo: Bool) -> String {
        undo
            ? String(localized: "Leal couldn’t undo “\(name)”.", comment: "Alert title: the core refused an undo; the action's name, such as Typing")
            : String(localized: "Leal couldn’t redo “\(name)”.", comment: "Alert title: the core refused a redo; the action's name, such as Typing")
    }

    static func undoRefusedDetail(_ refusal: EditRefusal) -> String {
        let reason = switch refusal {
        case .valueChanged, .otherLineage:
            String(localized: "The cells no longer hold what the change left in them.", comment: "Why an undo or redo was refused: the cells changed")
        default:
            EditText.refusal(refusal)
        }
        let cleared = String(localized: "The undo history was cleared. Your edits are still there.", comment: "After a refused undo or redo, the undo history is cleared")
        return "\(reason) \(cleared)"
    }

    /// Whether the core refused an undo or redo only for now (the file is
    /// still being read, a row can't be read yet, a save runs), not because
    /// the cells changed: the step is put back, to try again.
    static func isTemporary(_ refusal: EditRefusal) -> Bool {
        switch refusal {
        case .stillReading, .notReadYet, .unreadable, .saving:
            true
        case .noSuchRow, .tooFarRight, .afterUnterminatedQuote, .valueChanged, .otherLineage, .noSuchColumn:
            false
        }
    }

    /// Why an undo or redo can't happen yet, and that it is still there.
    static func undoNotYetDetail(_ refusal: EditRefusal) -> String {
        let reason = switch refusal {
        case .saving:
            String(localized: "The file is being saved.", comment: "Why an undo or redo can't happen yet: a save runs")
        case .unreadable:
            String(localized: "A row it changes can’t be read: its drive or share isn’t available.", comment: "Why an undo or redo can't happen yet: a row's drive or share went away")
        default:
            String(localized: "Leal is still reading the file.", comment: "Why an undo or redo can't happen yet: the file is still being read")
        }
        let again = String(localized: "Try again in a moment.", comment: "After an undo or redo refused for now: it is still in the Edit menu")
        return "\(reason) \(again)"
    }

    // MARK: Edits while the document is replaced

    static func editedDuringReload(_ name: String) -> String {
        String(localized: "Leal didn’t reload “\(name)”.", comment: "Alert title: a Reload wasn't adopted because the document was edited while the file was read; the file's name")
    }

    static let editedDuringReloadDetail = String(localized: "It was edited while the file was being read again, so your edits were kept. Reload again to discard them.", comment: "Alert text: a Reload wasn't adopted because of edits made meanwhile")

    static func editedDuringSaveAsUTF8(_ name: String) -> String {
        String(localized: "The UTF-8 copy was saved, but this window still shows “\(name)”.", comment: "Alert title: Save As UTF-8 saved the copy but the window didn't switch to it, because of edits made meanwhile; the original file's name")
    }

    static let editedDuringSaveAsUTF8Detail = String(localized: "It was edited during the save, so your edits were kept here. The copy doesn’t have the edits made during the save.", comment: "Alert text: Save As UTF-8 kept the window on the original because of edits made during the save")

    // MARK: Reload with edits (ADR-0008 decision 4)

    static func discardForReload(_ name: String) -> String {
        String(localized: "Reload “\(name)” and discard your changes?", comment: "Alert title: Reload would throw unsaved edits away; the file's name")
    }

    static let discardForReloadDetail = String(localized: "Your unsaved edits will be lost. This can’t be undone.", comment: "Alert text: Reload would throw unsaved edits away")
    static let reload = String(localized: "Reload", comment: "Button: reload and discard the edits")
    static let cancel = String(localized: "Cancel", comment: "Button: cancel")

    // MARK: Recover changes (ADR-0008 decision 5)

    static let recoverChanges = String(localized: "Recover Changes", comment: "Button: open the file again after a failure and put the unsaved edits back (ADR-0008 decision 5)")
    static let recoverChangesDetail = String(localized: "Recover Changes opens the file again and puts your unsaved edits back. Reopen and Close discard them.", comment: "Alert text after a failure, when there are unsaved edits")

    static let recoveryReadingDoesNotFit = String(localized: "The file no longer reads with the delimiter and encoding your edits were made in.", comment: "Alert text: Recover changes couldn't read the file the way it was read when the edits were made")

    static func recovered(_ name: String, applied: Int, total: Int) -> String {
        applied == total
            ? String(localized: "Leal put your changes to “\(name)” back, but the file changed since it was opened.", comment: "Alert title after Recover changes: every edit applied, but the file changed; the file's name")
            : String(localized: "Leal put back \(applied) of your \(total) changes to “\(name)”.", comment: "Alert title after Recover changes when some edits couldn't be applied; counts, the file's name")
    }

    static let recoveredDetail = String(localized: "Save a copy of what was recovered to keep it.", comment: "Alert text after Recover changes, offering Save As")
    static let saveAs = String(localized: "Save As…", comment: "Button: save a copy of the recovered document")
    static let notNow = String(localized: "Not Now", comment: "Button: don't save the recovered document now")

    static func recoveryFailed(_ name: String) -> String {
        String(localized: "Leal couldn’t recover your changes to “\(name)”.", comment: "Alert title: Recover changes couldn't open the file; the file's name")
    }

    /// The edits Recover changes couldn't put back, one per line, at most
    /// `limit` of them.
    static func refused(_ refused: [(command: EditCommand, refusal: EditRefusal)], header: Bool, limit: Int = 8) -> String {
        var lines = refused.prefix(limit).map { "• \(describe($0.command, header: header)): \(reason($0.refusal))" }
        if refused.count > limit {
            let more = refused.count - limit
            lines.append(String(localized: "and \(more) more", comment: "After a list of edits that couldn't be recovered: how many more"))
        }
        return lines.joined(separator: "\n")
    }

    /// A command in words: where it was.
    static func describe(_ command: EditCommand, header: Bool) -> String {
        if let structural = command.structural {
            let rows = Int(structural.rowCount())
            if structural.isColumn() {
                let column = Int(structural.column() ?? 0) + 1
                return structural.inserts()
                    ? String(localized: "Inserting column \(column)", comment: "An edit that couldn't be recovered: a column insert; the column's number")
                    : String(localized: "Deleting column \(column)", comment: "An edit that couldn't be recovered: a column delete; the column's number")
            }
            let row = rowName(structural.firstRow(), header: header)
            return structural.inserts()
                ? String(localized: "Inserting \(rows) rows at \(row)", comment: "An edit that couldn't be recovered: rows inserted; how many, where")
                : String(localized: "Deleting \(rows) rows at \(row)", comment: "An edit that couldn't be recovered: rows deleted; how many, where")
        }
        guard let first = command.changes.first else { return "" }
        let place = String(localized: "\(rowName(first.row, header: header)), column \(Int(first.column) + 1)", comment: "A cell: its row, then its column number")
        guard command.changes.count > 1 else { return place }
        let more = command.changes.count - 1
        return String(localized: "\(place) and \(more) more cells", comment: "Several cells changed at once: the first, then how many more")
    }

    /// Row `row` (a logical row) as the grid numbers it.
    private static func rowName(_ row: UInt64, header: Bool) -> String {
        if header, row == 0 {
            return String(localized: "The header row", comment: "The file's first row while it is the header row")
        }
        let shown = header ? Int(row) : Int(row) + 1
        return String(localized: "Row \(shown)", comment: "A row, as the grid numbers it")
    }

    private static func reason(_ refusal: EditRefusal) -> String {
        switch refusal {
        case .valueChanged, .otherLineage:
            String(localized: "the cell changed", comment: "Why an edit couldn't be recovered: the file changed there")
        case .noSuchRow, .noSuchColumn, .tooFarRight:
            String(localized: "it isn’t in the file any more", comment: "Why an edit couldn't be recovered: the row or column is gone")
        case .notReadYet, .unreadable, .stillReading, .saving:
            String(localized: "the row couldn’t be read", comment: "Why an edit couldn't be recovered: its row couldn't be read")
        case .afterUnterminatedQuote:
            String(localized: "a quote earlier in the row is never closed", comment: "Why an edit couldn't be recovered: an unterminated quote (ADR-0004 decision 8)")
        }
    }
}
