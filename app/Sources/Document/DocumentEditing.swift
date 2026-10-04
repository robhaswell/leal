import Foundation
import LealFFI
import os

/// A cell the user can edit (task 2.5.1): one of the grid's, or one of the
/// header row's. The header row is file row 0 while the file has one, and
/// isn't a grid row (docs/tasks/2.1.md, "The header row").
enum EditPlace: Hashable, Sendable {
    case cell(CellPosition)
    case header(column: Int)

    var column: Int {
        switch self {
        case let .cell(cell): cell.column
        case let .header(column): column
        }
    }
}

/// What committing an edit did.
enum EditOutcome {
    /// The cell reads as the new value now; the command is what undo needs.
    case edited(EditCommand)
    /// The cell already read as the value: no edit (ADR-0008 decision 3).
    case unchanged
    /// The core refused it, and nothing changed.
    case refused(EditRefusal)
    /// The call failed (the document failed, or a read error, which `call`
    /// has shown or logged).
    case failed
}

/// Editing a cell (task 2.5.1): the core's edit calls (task 2.1), with grid
/// rows turned into the core's logical rows.
extension DocumentModel {
    /// The core's logical row of `place`: a grid row is after the header
    /// row, if there is one.
    func logicalRow(_ place: EditPlace) -> UInt64 {
        switch place {
        case let .cell(cell): UInt64(cell.row + headerRows)
        case .header: 0
        }
    }

    /// Why `place` can't be edited, or `nil` if it can, for opening the
    /// in-cell editor and the inspector's editing (docs/tasks/2.1.md).
    /// UTF-16 files can be edited: only Save is off for them (ADR-0013
    /// decision 1). A failed document can't be edited at all.
    func editRefusal(_ place: EditPlace) -> EditRefusal? {
        if case .header = place, !interpretation.header { return .noSuchRow }
        if case let .cell(cell) = place, cell.row < 0 { return .noSuchRow }
        guard place.column >= 0 else { return .noSuchRow }
        let row = logicalRow(place)
        let refusal: EditRefusal?? = call { try $0.canEdit(row: row, column: UInt32(clamping: place.column)) }
        return refusal ?? .unreadable
    }

    /// The cell's whole display value, for the editors to start from
    /// (ADR-0008 decision 3): never the grid's shortened text or its
    /// symbols. `""` for a cell past the end of its row; `nil` if the row
    /// can't be read. On the main thread: for a value the grid shows whole
    /// (`isShownWhole`); a longer one is read with `fullValueInBackground`.
    func fullValue(_ place: EditPlace) -> String? {
        let row = logicalRow(place)
        let value: String?? = call { try $0.fullValue(row: row, column: UInt32(clamping: place.column)) }
        return value ?? nil
    }

    /// `fullValue(_:)` off the main thread, for a value that may be long,
    /// with whether the cell holds invalid bytes (`hasInvalidBytes`, read
    /// there too: it decodes the whole value). `nil` if the file was read
    /// again meanwhile, or the task was cancelled.
    ///
    /// Only the detached read holds the core's document, and only while a
    /// call into the core runs: cancelling the task (closing the window, a
    /// new edit) stops it before its next call, and this returns `nil`
    /// without keeping the document.
    func fullValueInBackground(_ place: EditPlace) async -> EditStart? {
        guard let read = startFullValueRead(place) else { return nil }
        let reading = readingID
        let result = await withTaskCancellationHandler {
            await read.value
        } onCancel: {
            read.cancel()
        }
        guard !Task.isCancelled, reading == readingID, !isFailed else { return nil }
        switch result {
        case let .success(start):
            return start
        case let .failure(error):
            let _: Void? = call { _ -> Void in throw error }
            return nil
        }
    }

    /// The detached read for `fullValueInBackground`, which alone holds the
    /// core's document (`handle`), until it returns.
    private func startFullValueRead(_ place: EditPlace) -> Task<Result<EditStart?, any Error>, Never>? {
        guard let handle = backgroundHandle() else { return nil }
        let row = logicalRow(place)
        let column = UInt32(clamping: place.column)
        return Task.detached(priority: .userInitiated) { () -> Result<EditStart?, any Error> in
            Result { () throws -> EditStart? in
                guard !Task.isCancelled, let value = try handle.fullValue(row: row, column: column) else { return nil }
                guard value.unicodeScalars.contains("\u{FFFD}"), !Task.isCancelled else {
                    return EditStart(value: value, invalid: false)
                }
                let cell = try handle.cellValue(row: row, column: column, maxChars: 0)
                return EditStart(value: value, invalid: cell?.invalid ?? false)
            }
        }
    }

    /// Whether the grid shows `place`'s whole value (it isn't cut at
    /// `GridMetrics.maxCellCharacters`), so its full value is short enough
    /// to read on the main thread.
    func isShownWhole(_ place: EditPlace) -> Bool {
        switch place {
        case let .cell(cell):
            switch cachedCell(row: cell.row, column: cell.column) ?? self.cell(row: cell.row, column: cell.column) {
            case let .text(_, truncated): !truncated
            case .missing: true
            case .notLoaded: false
            }
        case .header:
            // The titles are cut at the same length as cells: read the
            // value in full off the main thread only if one is that long.
            headerTitle(column: place.column).text.count < Int(GridMetrics.maxCellCharacters)
        }
    }

    /// Whether the cell holds bytes that aren't text in the file's
    /// encoding, shown as U+FFFD (mockup 05b): for the callout that says
    /// committing replaces them. `value` is its full value (never the
    /// grid's shortened text); only a value with a U+FFFD in it is asked
    /// about. On the main thread only for a value the grid shows whole: the
    /// core decodes the whole value (a long one: `fullValueInBackground`).
    func hasInvalidBytes(_ place: EditPlace, value: String) -> Bool {
        guard value.unicodeScalars.contains("\u{FFFD}") else { return false }
        let row = logicalRow(place)
        let cell: CellValue?? = call { try $0.cellValue(row: row, column: UInt32(clamping: place.column), maxChars: 0) }
        return (cell ?? nil)?.invalid ?? false
    }

    /// The first character of `value` the file's encoding can't hold, or
    /// `nil` (task 2.3): Save would refuse it, and Save As UTF-8 writes it.
    func unencodable(_ value: String) -> UnencodableCharacter? {
        let found: UnencodableCharacter?? = call { try $0.unencodable(value: value) }
        return found ?? nil
    }

    /// The file's main line ending, for a line break typed into a value
    /// (⌥↩ in the in-cell editor, Return in the inspector): `\n` until the
    /// core reports one.
    var lineBreak: String {
        switch lineEnding {
        case .crlf: "\r\n"
        case .cr: "\r"
        case .lf, nil: "\n"
        }
    }

    /// Commits an edit: `place` reads as `value` afterwards. Everything that
    /// shows the cell catches up (`valuesChanged`), and the command goes to
    /// `commandApplied`, the one place undo hears of it.
    func setCell(_ place: EditPlace, to value: String) -> EditOutcome {
        if let refusal = refusalForTesting { return .refused(refusal) }
        guard failure == nil, let handle = backgroundHandle() else { return .failed }
        let row = logicalRow(place)
        let command: EditCommand?
        do {
            command = try handle.setCell(row: row, column: UInt32(clamping: place.column), value: value)
        } catch let LealError.EditRefused(_, refusal, _, _) {
            return .refused(refusal)
        } catch {
            report(error)
            return .failed
        }
        guard let command else { return .unchanged }
        commandApplied(command, as: .edit)
        return .edited(command)
    }

    /// Undoes `command` (task 2.5.2): the core applies its inverse, and it
    /// goes to `commandApplied` as `.undo`. The core refuses it if a cell
    /// no longer holds what the command made, or (for rows and columns)
    /// while a save runs.
    func undo(_ command: EditCommand) -> EditOutcome {
        apply(command, as: .undo) { try $0.undo(command: command) }
    }

    /// Redoes `command`: as `undo`, the other way.
    func redo(_ command: EditCommand) -> EditOutcome {
        apply(command, as: .redo) { try $0.redo(command: command) }
    }

    private func apply(_ command: EditCommand, as direction: CommandDirection, _ body: (LealFFI.Document) throws -> Void) -> EditOutcome {
        guard failure == nil, let handle = backgroundHandle() else { return .failed }
        do {
            try body(handle)
        } catch let LealError.EditRefused(_, refusal, _, _) {
            return .refused(refusal)
        } catch {
            report(error)
            return .failed
        }
        commandApplied(command, as: direction)
        return .edited(command)
    }

    /// The core's edit version (2.2): it goes up with every command, never
    /// down. 0 once the document has failed.
    var editVersion: UInt64 {
        call { try $0.editVersion() } ?? 0
    }

    /// How this reading splits the file, for the recovery journal.
    var choices: ReadingChoices { ReadingChoices(interpretation) }

    /// A command was applied to the core's document, as an edit, or undone
    /// or redone: everything showing its cells catches up (`valuesChanged`,
    /// which after an undo measures the old values; `structureChanged` for
    /// rows and columns), the dirty state is read again, and `onCommand`
    /// hears of it, with which way it went.
    ///
    /// Every command goes through here, and only here: the document's
    /// `onCommand` registers its undo or redo and appends it to the
    /// recovery journal (task 2.5.2).
    func commandApplied(_ command: EditCommand, as direction: CommandDirection) {
        // First, so the window's status (Treat As off) is up to date when
        // it hears of the cells.
        refreshUnsavedEdits()
        if let structural = command.structural {
            structureChanged(by: structural, direction: direction)
        } else {
            valuesChanged(by: command, direction: direction)
        }
        onCommand?(command, direction)
    }
}

/// Which way a command was applied (`commandApplied`).
enum CommandDirection: Sendable {
    /// The command was made: its cells read as their `newValue`s.
    case edit
    /// Undone: its cells read as their `oldValue`s again.
    case undo
    /// Redone: as `edit`.
    case redo
}

/// A cell's full value read for editing (`fullValueInBackground`).
struct EditStart: Sendable {
    let value: String
    /// The cell holds bytes that aren't text in the file's encoding.
    let invalid: Bool
}

extension String {
    /// Whether `other` is this text exactly, scalar for scalar. Swift's `==`
    /// treats canonically equivalent text as equal (an NFC "é" and an NFD
    /// "e" with U+0301), which for an edit would drop a change to the
    /// bytes.
    func isIdentical(to other: String?) -> Bool {
        guard let other else { return false }
        return utf8.elementsEqual(other.utf8)
    }
}
