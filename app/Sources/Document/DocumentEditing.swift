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

    /// `fullValue(_:)` off the main thread, for a value that may be long.
    /// `nil` if the file was read again meanwhile.
    func fullValueInBackground(_ place: EditPlace) async -> String? {
        guard let handle = backgroundHandle() else { return nil }
        let row = logicalRow(place)
        let column = UInt32(clamping: place.column)
        let reading = readingID
        let result = await Task.detached(priority: .userInitiated) { () -> Result<String?, any Error> in
            Result { try handle.fullValue(row: row, column: column) }
        }.value
        guard reading == readingID, !isFailed else { return nil }
        switch result {
        case let .success(value):
            return value
        case let .failure(error):
            let _: Void? = call { _ -> Void in throw error }
            return nil
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
    /// committing replaces them. `value` is its full value; only a value
    /// with a U+FFFD in it is asked about.
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

    /// Commits an edit: `place` reads as `value` afterwards. Everything that
    /// shows the cell catches up (`valuesChanged`), and the command goes to
    /// `commandApplied`, the one place undo hears of it.
    func setCell(_ place: EditPlace, to value: String) -> EditOutcome {
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
        commandApplied(command)
        return .edited(command)
    }

    /// A command was applied to the core's document: everything showing
    /// its cells catches up, and `onCommand` hears of it.
    ///
    /// SEAM(2.5.2): this is the one place an edit is registered: 2.5.2's
    /// `onCommand` registers its undo (whose undo and redo call
    /// `valuesChanged(by:)` after `undo(command:)` and `redo(command:)`)
    /// and appends it to the recovery journal.
    private func commandApplied(_ command: EditCommand) {
        valuesChanged(by: command)
        onCommand?(command)
    }
}
