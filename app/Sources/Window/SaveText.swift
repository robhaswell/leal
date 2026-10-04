import Foundation
import LealFFI

/// The words for saving (DESIGN §4.4: from the String Catalog). Task 2.3
/// has Save As UTF-8's (the UTF-16 banner's button, mockup 06a); task 2.5
/// words every other `SaveFailure`.
enum SaveText {
    /// An alert's title and text.
    struct Message: Equatable {
        let title: String
        let detail: String
    }

    /// Why Save As UTF-8 didn't save; `nil` if it was cancelled, which says
    /// nothing. Nothing was written either way. The core's own words
    /// (English) are for the log, never shown.
    static func saveAsUTF8Failure(_ failure: SaveFailure, headerRows: Int) -> Message? {
        let title = String(
            localized: "The UTF-8 copy wasn’t saved.",
            comment: "Alert title: Save As UTF-8 failed (task 2.3)"
        )
        let detail: String
        switch failure {
        case .Cancelled:
            return nil
        case let .Unconvertible(encoding, cells, more):
            // ADR-0008 decision 7, F5: named, never replaced.
            let name = StatusText.encodingName(encoding)
            if let place = cells.first {
                detail = unconvertible(cells.count, more: more, first: cell(place, headerRows: headerRows), encoding: name)
            } else {
                // The core always names one; said without a place anyway.
                detail = String(
                    localized: "Some of the file’s bytes aren’t \(name) text, so they can’t be converted. Leal never replaces them.",
                    comment: "Alert text: Save As UTF-8 refused cells without saying which; the file's encoding"
                )
            }
        case .TooLarge:
            detail = String(
                localized: "The copy would be too large for Leal to open again.",
                comment: "Alert text: Save As UTF-8's file would be over 4 GiB (ADR-0012 decision 2)"
            )
        case .NotAFile:
            detail = String(
                localized: "Something other than a file is at that place.",
                comment: "Alert text: Save As UTF-8 onto a folder or similar"
            )
        case .Locked:
            detail = String(
                localized: "The file at that place is locked.",
                comment: "Alert text: Save As UTF-8 onto a locked file"
            )
        case .NotWritable:
            detail = String(
                localized: "Leal may not write the file at that place.",
                comment: "Alert text: Save As UTF-8 onto a file Leal may not write"
            )
        case .Io:
            detail = String(
                localized: "Leal couldn’t write the copy there. Check that the drive has room and that you may write to that folder.",
                comment: "Alert text: Save As UTF-8 failed writing the file (a full disk, no permission); the details are in the log"
            )
        case .ChangedElsewhere:
            detail = String(
                localized: "The file at that place changed while Leal was saving. Try again.",
                comment: "Alert text: Save As UTF-8's destination changed during the save"
            )
        case .DocumentFailed, .Internal:
            detail = String(
                localized: "Something went wrong inside Leal. Try again, or reopen the file first.",
                comment: "Alert text: Save As UTF-8 failed because of a problem in Leal itself; the details are in the log"
            )
        default:
            detail = String(
                localized: "Leal couldn’t read all of the file. Try again, or reopen it first.",
                comment: "Alert text: Save As UTF-8 failed for another reason"
            )
        }
        return Message(title: title, detail: detail)
    }

    /// The cells Save As UTF-8 refused: `count` of them (or more, if
    /// `more`), the first at `first`.
    private static func unconvertible(_ count: Int, more: Bool, first: String, encoding name: String) -> String {
        if count == 1 && !more {
            return String(
                localized: "The cell at \(first) holds bytes that aren’t \(name) text, so they can’t be converted. Leal never replaces them.",
                comment: "Alert text: Save As UTF-8 refused one cell; where it is, as in \"row 3, column 2\", and the file's encoding"
            )
        }
        let counted = more ? String(localized: "More than \(count.formatted())", comment: "Alert text: how many cells Save As UTF-8 refused, when it stopped counting") : count.formatted()
        return String(
            localized: "\(counted) cells hold bytes that aren’t \(name) text, the first at \(first), so they can’t be converted. Leal never replaces them.",
            comment: "Alert text: Save As UTF-8 refused several cells; how many, the file's encoding, and where the first is"
        )
    }

    /// Where a cell is, in the gutter's numbers: "row 3, column 2", or "the
    /// header row, column 2".
    static func cell(_ place: CellPlace, headerRows: Int) -> String {
        let row = DiagnosticsText.row(place.row, headerRows: headerRows)
        let column = Int(place.column) + 1
        return String(
            localized: "\(row), column \(column.formatted())",
            comment: "Where a cell is: its row (as in \"row 3\" or \"the header row\"), then its column number"
        )
    }
}

// MARK: Save (task 2.5.3a)

/// What the user can do when Save stops (task 2.5.3a): each a button of
/// the alert that says why.
enum SaveChoice: Equatable, Sendable {
    /// Write over a file that changed elsewhere (`overwriteChanged`).
    case saveAnyway
    /// Clear the file's locked flag, then save again.
    case unlock
    /// Keep the changes in a copy elsewhere.
    case duplicate
    /// Save a UTF-8 copy (ADR-0008 decision 7).
    case saveAsUTF8
    /// Save elsewhere.
    case saveAs
    /// Save again.
    case tryAgain
    case cancel
    case ok
}

extension SaveText {
    /// Why Save stopped, or what it asks before writing, and the choices
    /// it offers, the first being the default and the last the way out.
    struct Refusal: Equatable {
        let title: String
        let detail: String
        let choices: [SaveChoice]
        /// The failure's particulars, shown in small type under the text:
        /// for a write that failed, which step and the OS error, so a
        /// report from the user names the cause.
        var details: String?
    }

    /// The particulars of a failed write: the core's step (English, as in
    /// the log) and the OS error's number and description.
    static func ioDetails(step: String, code: Int32?, message: String) -> String {
        let error = code.map { "errno \($0), \(String(cString: strerror($0)))" } ?? message
        return String(
            localized: "Details: \(step) failed (\(error)).",
            comment: "Alert text, small type: which step of a save failed, in English as the log has it, and the OS error's number and description"
        )
    }

    /// The most cells an alert names one by one; the rest are counted.
    static let namedCells = 8

    /// What Save asks before writing over a file that changed elsewhere:
    /// the watcher saw a change (`OriginalStatus.diverged`, which stays set
    /// after Keep Editing), or the core's check before writing found one
    /// (`ChangedElsewhere`). Asked once, never with `NSDocument`'s own.
    static func changedElsewhere(name: String) -> Refusal {
        Refusal(
            title: String(
                localized: "“\(name)” changed on disk since Leal opened or last saved it.",
                comment: "Alert title: Save would write over a file another app changed; the file's name"
            ),
            detail: String(
                localized: "Another app changed or replaced it. Saving now replaces that version with what Leal shows.",
                comment: "Alert text: Save would write over a file another app changed"
            ),
            choices: [.saveAnyway, .cancel]
        )
    }

    /// Why Save didn't write the file, and what the user can do; `nil` if
    /// it says nothing (cancelled, or the document failed, which its own
    /// alert says). Nothing was written either way. The core's own words
    /// (English) are for the log, never shown.
    static func saveFailure(_ failure: SaveFailure, name: String, headerRows: Int) -> Refusal? {
        let title = String(localized: "“\(name)” wasn’t saved.", comment: "Alert title: Save failed; the file's name")
        switch failure {
        case .Cancelled, .DocumentFailed:
            return nil
        case .ChangedElsewhere:
            return changedElsewhere(name: name)
        case .Locked:
            return Refusal(
                title: String(localized: "“\(name)” is locked.", comment: "Alert title: Save refused a locked file; the file's name"),
                detail: String(
                    localized: "Unlock it to save your changes there, or keep them in a duplicate.",
                    comment: "Alert text: Save refused a locked file; the buttons are Unlock, Duplicate and Cancel"
                ),
                choices: [.unlock, .duplicate, .cancel]
            )
        case .NotWritable:
            return Refusal(
                title: String(
                    localized: "You don’t have permission to save “\(name)”.",
                    comment: "Alert title: Save refused a file Leal may not write; the file's name"
                ),
                detail: String(
                    localized: "Keep your changes in a duplicate, or ask the file’s owner for permission to write it.",
                    comment: "Alert text: Save refused a file Leal may not write; the buttons are Duplicate and Cancel"
                ),
                choices: [.duplicate, .cancel]
            )
        case let .Unencodable(encoding, cells, more):
            return unencodable(name: name, encoding: StatusText.encodingName(encoding), cells: cells, more: more, headerRows: headerRows)
        case .Missing:
            return Refusal(
                title: String(localized: "“\(name)” can’t be found.", comment: "Alert title: Save found no file where it was; the file's name"),
                detail: String(
                    localized: "It was moved, renamed or deleted since Leal opened it. Save As keeps your changes in a new file.",
                    comment: "Alert text: Save found no file where it was; the buttons are Save As… and Cancel"
                ),
                choices: [.saveAs, .cancel]
            )
        case .Moving:
            return Refusal(
                title: title,
                detail: String(
                    localized: "The file is being moved or replaced, perhaps by another app. Try again in a moment.",
                    comment: "Alert text: Save found the file in the middle of a rename; the buttons are Try Again and Cancel"
                ),
                choices: [.tryAgain, .cancel]
            )
        case .Unavailable:
            return Refusal(
                title: title,
                detail: String(
                    localized: "The drive or server it’s on isn’t connected. Connect it and try again, or use Save As to keep your changes elsewhere.",
                    comment: "Alert text: Save found the file's volume not mounted; the buttons are Save As… and Cancel"
                ),
                choices: [.saveAs, .cancel]
            )
        case .Incomplete, .DriveDisconnected, .ChangedOnDisk, .DeletedElsewhere:
            return Refusal(
                title: title,
                detail: String(
                    localized: "Leal doesn’t have all of the file, so saving over it would lose rows. Save As keeps a copy of what Leal has.",
                    comment: "Alert text: Save refused because Leal couldn't read the whole file (its drive went, or it changed while read)"
                ),
                choices: [.saveAs, .cancel]
            )
        case .ReadOnly:
            return Refusal(
                title: title,
                detail: String(
                    localized: "Leal saves UTF-16 files only as a UTF-8 copy.",
                    comment: "Alert text: Save of a UTF-16 file (ADR-0013 decision 1); the buttons are Save As UTF-8… and Cancel"
                ),
                choices: [.saveAsUTF8, .cancel]
            )
        case .TooLarge:
            return Refusal(
                title: title,
                detail: String(
                    localized: "The file would be too large for Leal to open again.",
                    comment: "Alert text: Save's file would be over 4 GiB (ADR-0012 decision 2)"
                ),
                choices: [.ok]
            )
        case .NotAFile:
            return Refusal(
                title: title,
                detail: String(
                    localized: "Something other than a file, such as a folder, is where the file was.",
                    comment: "Alert text: Save found a folder or similar at the file's place"
                ),
                choices: [.ok]
            )
        case let .Io(step, code, message):
            return Refusal(
                title: title,
                detail: String(
                    localized: "Leal couldn’t write the file. Check that the drive has room and that you may write to that folder.",
                    comment: "Alert text: Save failed writing the file (a full disk, no permission); the details are in the log"
                ),
                choices: [.ok],
                details: ioDetails(step: step, code: code, message: message)
            )
        case .Unconvertible, .Internal:
            return Refusal(
                title: title,
                detail: String(
                    localized: "Something went wrong inside Leal. Try again, or reopen the file first.",
                    comment: "Alert text: Save failed because of a problem in Leal itself; the details are in the log"
                ),
                choices: [.ok]
            )
        }
    }

    /// Save refused values the file's encoding can't hold (F5): the cells,
    /// the first `namedCells` one by one, then how many more ("and more"
    /// once the core stopped counting, at 1,000), and Save As UTF-8.
    private static func unencodable(name: String, encoding: String, cells: [CellPlace], more: Bool, headerRows: Int) -> Refusal {
        let title = String(
            localized: "Some cells can’t be saved in \(encoding).",
            comment: "Alert title: Save refused values the file's encoding can't represent; the encoding"
        )
        var lines = cells.prefix(namedCells).map { "• " + capitalized(cell($0, headerRows: headerRows)) }
        let rest = cells.count - min(cells.count, namedCells)
        if more {
            lines.append(String(localized: "and more", comment: "After a list of cells Save refused, when there are more than Leal counted"))
        } else if rest > 0 {
            lines.append(String(localized: "and \(rest) more", comment: "After a list of cells Save refused: how many more"))
        }
        let why = String(
            localized: "They hold characters \(encoding) can’t represent, and Leal never replaces them. Change them, or save a UTF-8 copy of “\(name)”.",
            comment: "Alert text after the cells Save refused; the encoding and the file's name; the buttons are Save As UTF-8… and Cancel"
        )
        return Refusal(title: title, detail: (lines + ["", why]).joined(separator: "\n"), choices: [.saveAsUTF8, .cancel])
    }

    /// `text` with its first letter capitalised, for the start of a line.
    private static func capitalized(_ text: String) -> String {
        guard let first = text.first else { return text }
        return first.uppercased() + text.dropFirst()
    }

    /// A choice's button.
    static func button(_ choice: SaveChoice) -> String {
        switch choice {
        case .saveAnyway: String(localized: "Save Anyway", comment: "Button: write over a file another app changed")
        case .unlock: String(localized: "Unlock", comment: "Button: unlock a locked file, then save")
        case .duplicate: String(localized: "Duplicate", comment: "Button: keep the changes in a copy of a file Leal can't save")
        case .saveAsUTF8: String(localized: "Save As UTF-8…", comment: "Button: save a UTF-8 copy (ADR-0008 decision 7)")
        case .saveAs: String(localized: "Save As…", comment: "Button: save the document elsewhere")
        case .tryAgain: String(localized: "Try Again", comment: "Button: save again")
        case .cancel: String(localized: "Cancel", comment: "Button: don't save")
        case .ok: String(localized: "OK", comment: "Button: dismiss an alert")
        }
    }

    /// Unlocking the file failed.
    /// Save refused a file locked by the system (`schg` or `sappnd`), which
    /// only an administrator can unlock: no Unlock.
    static func lockedBySystem(name: String) -> Refusal {
        Refusal(
            title: String(localized: "“\(name)” is locked.", comment: "Alert title: Save refused a locked file; the file's name"),
            detail: String(
                localized: "Only an administrator can unlock it. Keep your changes in a duplicate instead.",
                comment: "Alert text: Save refused a file the system locked (schg); the buttons are Duplicate and Cancel"
            ),
            choices: [.duplicate, .cancel]
        )
    }

    static func unlockFailed(name: String) -> Refusal {
        Refusal(
            title: String(localized: "Leal couldn’t unlock “\(name)”.", comment: "Alert title: Unlock failed; the file's name"),
            detail: String(
                localized: "You may not be allowed to change it. Keep your changes in a duplicate instead.",
                comment: "Alert text: Unlock failed; the buttons are Duplicate and Cancel"
            ),
            choices: [.duplicate, .cancel]
        )
    }

    // MARK: After a save (task 2.5.3b)

    /// The save kept the file it replaced (`SaveOutcome.keptOldFile`): the
    /// swap took out a file it couldn't check, which may be another app's
    /// version. Leal moved it to its Recovered folder as `keptAs`.
    static func keptOldFile(name: String, keptAs: String) -> Message {
        Message(
            title: keptTitle(name: name),
            detail: String(
                localized: "Another app may have changed the file as Leal saved it. Its version is in Leal’s Recovered folder, as “\(keptAs)”.",
                comment: "Alert text: where the kept old file now is; the name it was kept as"
            )
        )
    }

    /// As `keptOldFile`, but the core kept it next to the file saved, as
    /// `keptAs` (its first choice), where it stays.
    static func keptOldFileBeside(name: String, keptAs: String) -> Message {
        Message(
            title: keptTitle(name: name),
            detail: String(
                localized: "Another app may have changed the file as Leal saved it. Its version is next to it, as “\(keptAs)”.",
                comment: "Alert text: the kept old file is in the same folder as the file saved; the name it was kept as"
            )
        )
    }

    /// As `keptOldFile`, but it couldn't be moved to the Recovered folder:
    /// it is still in the save's temporary folder, at `path`.
    static func keptOldFileNotMoved(name: String, path: String) -> Message {
        Message(
            title: keptTitle(name: name),
            detail: String(
                localized: "Another app may have changed the file as Leal saved it. Its version is at \(path), in a temporary folder: move it somewhere safe.",
                comment: "Alert text: the kept old file couldn't be moved to the Recovered folder; where it is"
            )
        )
    }

    private static func keptTitle(name: String) -> String {
        String(
            localized: "Leal saved “\(name)” and kept the version it replaced.",
            comment: "Alert title: a save kept the old file, which may hold another app's changes; the file's name"
        )
    }

    /// The button that shows the kept file in Finder.
    static var showInFinder: String {
        String(localized: "Show in Finder", comment: "Button: show the file a save kept in Finder")
    }
}

// MARK: Save As (task 2.5.3c)

extension SaveText {
    /// Why Save As didn't save, and what the user can do; `nil` if it says
    /// nothing (cancelled, or the document failed, which its own alert
    /// says). Nothing was written either way. Text the file's encoding
    /// can't hold, and a UTF-16 file, offer Save As UTF-8; the rest say why
    /// (in Save As UTF-8's words) with OK.
    static func saveAsFailure(_ failure: SaveFailure, name: String, headerRows: Int) -> Refusal? {
        switch failure {
        case .Cancelled, .DocumentFailed:
            return nil
        case .Unencodable, .ReadOnly:
            return saveFailure(failure, name: name, headerRows: headerRows)
        default:
            guard let message = saveAsUTF8Failure(failure, headerRows: headerRows) else { return nil }
            return Refusal(
                title: String(localized: "“\(name)” wasn’t saved.", comment: "Alert title: Save failed; the file's name"),
                detail: message.detail,
                choices: [.ok]
            )
        }
    }

    /// The save panel's message when the copy will be incomplete (ADR-0008
    /// decision 6): Leal has read about `rows` of `total` rows (a total no
    /// larger is unknown: the file went before Leal could tell).
    static func incompleteCopyMessage(rows: Int, of total: Int) -> String {
        if total > rows {
            String(
                localized: "Leal has read only about \(rows.formatted()) of \(total.formatted()) rows, so this copy will be incomplete: it gets only the rows Leal has read in full.",
                comment: "Save panel message: Save As from a document Leal couldn't read all of (its drive or share went, or the file changed or was deleted while read); rows read, rows in the file"
            )
        } else {
            String(
                localized: "Leal couldn’t read all of the file, so this copy will be incomplete: it gets only the \(rows.formatted()) rows Leal has read in full.",
                comment: "Save panel message: as the other, when Leal can't tell how many rows the file has; rows read"
            )
        }
    }

    /// After an incomplete Save As (ADR-0008 decision 6): it has `rows` of
    /// about `total` rows, and the edits to rows it doesn't have
    /// (`skipped`) weren't saved, named as Save's unencodable cells are.
    static func incompleteCopy(name: String, rows: Int, of total: Int, skipped: [CellPlace], headerRows: Int) -> Refusal {
        var detail = if total > rows {
            String(
                localized: "It has about \(rows.formatted()) of \(total.formatted()) rows: only the rows Leal had read in full.",
                comment: "Alert text after Save As saved an incomplete copy; rows written, rows in the file"
            )
        } else {
            String(
                localized: "It has only the \(rows.formatted()) rows Leal had read in full, not the whole file.",
                comment: "Alert text after Save As saved an incomplete copy, when Leal can't tell how many rows the file has; rows written"
            )
        }
        if !skipped.isEmpty {
            var lines = skipped.prefix(namedCells).map { "• " + capitalized(cell($0, headerRows: headerRows)) }
            let rest = skipped.count - min(skipped.count, namedCells)
            if rest > 0 {
                lines.append(String(localized: "and \(rest) more", comment: "After a list of cells Save refused: how many more"))
            }
            let why = String(
                localized: "These edits weren’t saved, because their rows aren’t in the copy:",
                comment: "Alert text after an incomplete Save As, before the list of edited cells it didn't save"
            )
            detail += "\n\n" + ([why] + lines).joined(separator: "\n")
        }
        return Refusal(
            title: String(
                localized: "The copy “\(name)” is incomplete.",
                comment: "Alert title: Save As saved an incomplete copy (ADR-0008 decision 6); the copy's name"
            ),
            detail: detail,
            choices: [.ok]
        )
    }

    /// The name Duplicate suggests: the file's own with "copy", as the
    /// Finder names a duplicate.
    static func copyName(of url: URL) -> String {
        let stem = url.deletingPathExtension().lastPathComponent
        let name = String(localized: "\(stem) copy", comment: "Duplicate: the suggested name, as in \"people copy\"; the file's name without its extension")
        return url.pathExtension.isEmpty ? name : "\(name).\(url.pathExtension)"
    }
}
