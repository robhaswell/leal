import Foundation
import LealFFI

/// Inserting and deleting rows and columns (task 2.5a, DESIGN §4.2): the
/// core's structural edits (task 2.4), with grid rows turned into the
/// core's logical rows. Each command goes to `commandApplied`, as a cell
/// edit does, so undo, the journal, the grid, Find and the inspector hear
/// of it there.
///
/// The core decides whether one is allowed (ADR-0014 decision 1: not
/// until the whole file is read, nor while a save runs; ADR-0004 decision
/// 8: nothing after an unterminated quote); the app asks it first, to
/// disable the menu items, giving the reason.
extension DocumentModel {
    /// The core's logical row for grid row `gridRow`; past the last row,
    /// the end of the file.
    private func logicalRow(gridRow: Int) -> UInt64 {
        UInt64(max(0, gridRow) + headerRows)
    }

    // MARK: Whether they are allowed

    /// Why rows can't be inserted before grid row `gridRow` now (the row
    /// count: after the last row), or `nil` if they can. Before the first
    /// data row is after the header row: the header row stays the file's
    /// first (docs/tasks/2.4.md, "Edge cases").
    func rowInsertRefusal(beforeGridRow gridRow: Int) -> EditRefusal? {
        let at = logicalRow(gridRow: gridRow)
        let refusal: EditRefusal?? = call { try $0.canInsertRows(at: at) }
        return refusal ?? .unreadable
    }

    /// Why rows can't be deleted now, or `nil` if they can.
    func rowDeleteRefusal() -> EditRefusal? {
        let refusal: EditRefusal?? = call { try $0.canChangeRows() }
        return refusal ?? .unreadable
    }

    /// Why a column can't be inserted before column `column` now (one past
    /// the widest row: after the last), or `nil` if it can.
    func columnInsertRefusal(before column: Int) -> EditRefusal? {
        guard column >= 0 else { return .noSuchColumn }
        let refusal: EditRefusal?? = call { try $0.canInsertColumn(at: UInt32(clamping: column)) }
        return refusal ?? .unreadable
    }

    /// Why column `column` can't be deleted now, or `nil` if it can.
    func columnDeleteRefusal(_ column: Int) -> EditRefusal? {
        guard column >= 0 else { return .noSuchColumn }
        let refusal: EditRefusal?? = call { try $0.canDeleteColumn(at: UInt32(clamping: column)) }
        return refusal ?? .unreadable
    }

    // MARK: Commands

    /// Inserts one row of empty cells before grid row `gridRow` (the row
    /// count appends it). It has as many cells as most rows (the core's
    /// column count), so it isn't a short row; in a file with no columns,
    /// one.
    func insertRow(beforeGridRow gridRow: Int) -> EditOutcome {
        let at = logicalRow(gridRow: gridRow)
        let row = Array(repeating: "", count: max(1, fileColumnCount))
        return make { try $0.insertRows(at: at, rows: [row]) }
    }

    /// Deletes grid rows `rows`, as one command.
    func deleteRows(_ rows: ClosedRange<Int>) -> EditOutcome {
        let at = logicalRow(gridRow: rows.lowerBound)
        let count = UInt64(rows.count)
        return make { try $0.deleteRows(at: at, count: count) }
    }

    /// Inserts a column of empty cells before column `column`, in every
    /// row that reaches it (a shorter row is left as it is). With a header
    /// row, its new title is empty too.
    func insertColumn(before column: Int) -> EditOutcome {
        make { try $0.insertColumn(at: UInt32(clamping: column), value: "") }
    }

    /// Deletes column `column` from every row that has it.
    func deleteColumn(_ column: Int) -> EditOutcome {
        make { try $0.deleteColumn(at: UInt32(clamping: column)) }
    }

    /// Makes a command in the core, and hands it to `commandApplied`.
    private func make(_ body: (LealFFI.Document) throws -> EditCommand?) -> EditOutcome {
        guard failure == nil, let handle = backgroundHandle() else { return .failed }
        let command: EditCommand?
        do {
            command = try body(handle)
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
}

/// Where rows or a column were inserted or deleted (`DocumentChange.structure`),
/// as applied: after an undo, the other way round from the command. The
/// window puts the selection there, and Find restarts after a column's
/// change but catches up after rows' (ADR-0014 decision 2).
struct StructureChange: Equatable, Sendable {
    /// The column, for a column's insert or delete; `nil` for rows.
    let column: Int?
    /// The first row's grid row, for rows (−1 for the header row).
    let row: Int
    /// Whether it inserted, rather than deleted.
    let inserted: Bool

    var isColumn: Bool { column != nil }
}
