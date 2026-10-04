import AppKit
import LealFFI

/// The Edit menu's row and column commands (task 2.5a, DESIGN §4.2).
enum StructureCommand: CaseIterable {
    case insertRowAbove
    /// ⌘↩: a new row below the active cell's, to type into.
    case insertRowBelow
    /// ⇧⌘↩: a copy of each selected row, after the last.
    case duplicateRows
    /// ⌘⌫: the selected rows.
    case deleteRows
    case insertColumnBefore
    case insertColumnAfter
    /// The selected columns.
    case deleteColumns

    /// The command a row command's key runs.
    init(_ key: RowCommandKey) {
        switch key {
        case .insertBelow: self = .insertRowBelow
        case .duplicate: self = .duplicateRows
        case .delete: self = .deleteRows
        }
    }

    var isColumn: Bool {
        switch self {
        case .insertRowAbove, .insertRowBelow, .duplicateRows, .deleteRows: false
        case .insertColumnBefore, .insertColumnAfter, .deleteColumns: true
        }
    }
}

/// Whether a row or column command can run now, and if not, why
/// (`DocumentViewController.availability`).
struct StructureAvailability: Equatable {
    let enabled: Bool
    /// What the menu item's tooltip says, and VoiceOver hears if its key
    /// is pressed: only for a reason the user can wait out or act on.
    let reason: String?

    static let available = StructureAvailability(enabled: true, reason: nil)
    static let off = StructureAvailability(enabled: false, reason: nil)
}

/// Insert, duplicate and delete rows, and insert and delete columns (task
/// 2.5a): Edit menu commands, and ⌘↩, ⇧⌘↩ and ⌘⌫ (no other UI without a
/// mockup, ADR-0002). Each commits an open edit first, as other commands
/// do, and goes through the model's `commandApplied`, so it is undoable and
/// in the recovery journal; the window then selects where the change was
/// (`selectChange`): a new row's first cell, the copies in the columns
/// selected.
extension DocumentViewController {
    @objc func insertRowAbove(_ sender: Any?) { run(.insertRowAbove) }
    @objc func insertRowBelow(_ sender: Any?) { run(.insertRowBelow) }
    @objc func duplicateRows(_ sender: Any?) { run(.duplicateRows) }
    @objc func deleteRows(_ sender: Any?) {
        // A held ⌘⌫ deletes one row (or selection), not a row each repeat.
        if let event = NSApp.currentEvent, event.type == .keyDown, event.isARepeat { return }
        run(.deleteRows)
    }
    @objc func insertColumnBefore(_ sender: Any?) { run(.insertColumnBefore) }
    @objc func insertColumnAfter(_ sender: Any?) { run(.insertColumnAfter) }
    @objc func deleteColumns(_ sender: Any?) { run(.deleteColumns) }

    static func action(_ command: StructureCommand) -> Selector {
        switch command {
        case .insertRowAbove: #selector(insertRowAbove(_:))
        case .insertRowBelow: #selector(insertRowBelow(_:))
        case .duplicateRows: #selector(duplicateRows(_:))
        case .deleteRows: #selector(deleteRows(_:))
        case .insertColumnBefore: #selector(insertColumnBefore(_:))
        case .insertColumnAfter: #selector(insertColumnAfter(_:))
        case .deleteColumns: #selector(deleteColumns(_:))
        }
    }

    static func command(for action: Selector?) -> StructureCommand? {
        StructureCommand.allCases.first { Self.action($0) == action }
    }

    /// The grid rows Delete Row deletes and Duplicate Row copies: the
    /// selection's, within the file.
    var selectedRows: ClosedRange<Int>? {
        guard let rows = grid.selection?.rows, model.rowCount > 0, rows.lowerBound < model.rowCount else { return nil }
        return rows.lowerBound...min(rows.upperBound, model.rowCount - 1)
    }

    /// The columns Delete Column deletes: the selection's.
    var columnsToDelete: ClosedRange<Int>? {
        grid.selection?.columns
    }

    /// Whether `command` can run now, and if not, why: the core decides
    /// (ADR-0014 decision 1, ADR-0004 decision 8). Off as well while a
    /// text field other than the editors has the focus (the find bar): so
    /// ⌘⌫ and ⌘↩ are its own there.
    func availability(_ command: StructureCommand) -> StructureAvailability {
        guard !model.isFailed, !isReplacingDocument else { return .off }
        // The key window's too: a menu's action reaches this window from
        // a sheet over it (Go to Row's field).
        for window in [view.window, keyWindow()] {
            if let text = window?.firstResponder as? NSText, !(text is LiteralTextView) { return .off }
        }
        let cell = grid.activeCell
        let refusal: EditRefusal?
        switch command {
        case .insertRowAbove:
            refusal = model.rowInsertRefusal(beforeGridRow: cell?.row ?? model.rowCount)
        case .insertRowBelow:
            refusal = model.rowInsertRefusal(beforeGridRow: cell.map { $0.row + 1 } ?? model.rowCount)
        case .duplicateRows:
            guard let rows = selectedRows else { return .off }
            refusal = model.rowDuplicateRefusal(rows)
        case .deleteRows:
            guard selectedRows != nil else { return .off }
            refusal = model.rowDeleteRefusal()
        case .insertColumnBefore:
            guard let cell else { return .off }
            refusal = model.columnInsertRefusal(before: cell.column)
        case .insertColumnAfter:
            guard let cell else { return .off }
            refusal = model.columnInsertRefusal(before: cell.column + 1)
        case .deleteColumns:
            // A row with the last column has every column before it.
            guard let columns = columnsToDelete else { return .off }
            refusal = model.columnDeleteRefusal(columns.upperBound)
        }
        guard let refusal else { return .available }
        return StructureAvailability(enabled: false, reason: StructureText.reason(refusal, command))
    }

    /// The Edit menu's row and column items: on or off, with the reason as
    /// the tooltip (as Treat As gives its reason), and Delete Row's and
    /// Delete Column's titles for the number selected.
    func validateStructureItem(_ item: NSMenuItem, _ command: StructureCommand) -> Bool {
        let state = availability(command)
        item.toolTip = state.reason
        switch command {
        case .duplicateRows:
            item.title = (selectedRows?.count ?? 1) > 1 ? StructureText.duplicateRows : StructureText.duplicateRow
        case .deleteRows:
            item.title = (selectedRows?.count ?? 1) > 1 ? StructureText.deleteRows : StructureText.deleteRow
        case .deleteColumns:
            item.title = (columnsToDelete?.count ?? 1) > 1 ? StructureText.deleteColumns : StructureText.deleteColumn
        default:
            break
        }
        return state.enabled
    }

    /// ⌘↩, ⇧⌘↩ or ⌘⌫ reached the grid: its menu item was off (or ⌘ with
    /// the keypad's Enter, which isn't the item's key). It runs if it can
    /// now; otherwise a beep, and VoiceOver hears why.
    func rowCommandKey(_ command: StructureCommand) {
        let state = availability(command)
        guard state.enabled else {
            NSSound.beep()
            if let reason = state.reason { announce(reason) }
            return
        }
        run(command)
    }

    /// Runs `command`, after committing an open edit (one the core refuses
    /// stays open, saying why, and nothing else happens).
    func run(_ command: StructureCommand) {
        scheduler.noteUserInput()
        guard !model.isFailed, !isReplacingDocument, commitEditing() else { return NSSound.beep() }
        // The user chose where to work: a Next still waiting for the
        // search doesn't move the selection now.
        find.cancelPendingStep()
        let cell = grid.activeCell
        let outcome: EditOutcome
        switch command {
        case .insertRowAbove, .insertRowBelow:
            let row = cell.map { $0.row + (command == .insertRowBelow ? 1 : 0) } ?? model.rowCount
            outcome = model.insertRow(beforeGridRow: row)
            // The new row's first cell, to type into.
            if case .edited = outcome { grid.select(CellPosition(row: row, column: 0)) }
        case .duplicateRows:
            guard let rows = selectedRows, let selection = grid.selection else { return NSSound.beep() }
            outcome = duplicateAsOneStep(rows)
            // The copies, in the columns selected.
            if case .edited = outcome { grid.select(selection.copied(rows)) }
        case .deleteRows:
            guard let rows = selectedRows else { return NSSound.beep() }
            outcome = model.deleteRows(rows)
        case .insertColumnBefore, .insertColumnAfter:
            guard let cell else { return NSSound.beep() }
            outcome = model.insertColumn(before: cell.column + (command == .insertColumnAfter ? 1 : 0))
        case .deleteColumns:
            guard let columns = columnsToDelete else { return NSSound.beep() }
            outcome = deleteAsOneStep(columns)
        }
        if case let .refused(refusal) = outcome {
            NSSound.beep()
            if let reason = StructureText.reason(refusal, command) { announce(reason) }
        }
    }

    /// Duplicates `rows`, one command of the core's (a row insert), as an
    /// undo step named "Duplicate Row" or "Duplicate Rows"; undone and
    /// redone, the step keeps the name.
    private func duplicateAsOneStep(_ rows: ClosedRange<Int>) -> EditOutcome {
        var outcome = EditOutcome.unchanged
        asOneStep {
            outcome = model.duplicateRows(rows)
            return rows.count > 1 ? StructureText.duplicateRows : StructureText.duplicateRow
        }
        return outcome
    }

    /// Deletes `columns`, last first, each a command of the core's, as one
    /// undo step ("Delete Columns"). The last is asked about first, so a
    /// refusal deletes none. One the core refuses part-way stops the rest;
    /// those already deleted stay deleted, and undo as one.
    private func deleteAsOneStep(_ columns: ClosedRange<Int>) -> EditOutcome {
        guard columns.count > 1 else { return model.deleteColumn(columns.lowerBound) }
        if let refusal = model.columnDeleteRefusal(columns.upperBound) { return .refused(refusal) }
        var outcome = EditOutcome.unchanged
        asOneStep {
            var deleted = 0
            for column in columns.reversed() {
                outcome = model.deleteColumn(column)
                guard case .edited = outcome else { break }
                deleted += 1
            }
            return deleted > 1 ? StructureText.deleteColumns : nil
        }
        return outcome
    }

    /// Runs `body`'s commands as one step of the document's history (the
    /// window's undo manager, which a window not shown yet hasn't), named
    /// what `body` returns (`nil`: its last command's name). A `body` the
    /// core refused leaves no step: no nameless Undo, and Redo as it was
    /// (`DocumentUndoManager.registerAsOneStep`).
    private func asOneStep(_ body: () -> String?) {
        if let undo = undoHistory() {
            undo.registerAsOneStep(body)
        } else {
            _ = body()
        }
    }

    /// Selects where rows or a column were inserted or deleted (or put
    /// back, or taken out again, by undo and redo), and shows it: an
    /// inserted row's first cell in the active cell's column, or the row
    /// that took a deleted one's place (the last row, if they were the
    /// last); for a column, the same in the active cell's row.
    func selectChange(_ change: StructureChange) {
        let rows = model.rowCount
        let columns = model.columnCount
        guard rows > 0, columns > 0 else {
            grid.activeCell = nil
            return
        }
        let current = grid.activeCell ?? CellPosition(row: 0, column: 0)
        let cell = if let column = change.column {
            CellPosition(row: min(current.row, rows - 1), column: min(max(0, column), columns - 1))
        } else {
            CellPosition(row: min(max(0, change.row), rows - 1), column: min(current.column, columns - 1))
        }
        grid.select(cell)
    }
}

/// The words of the row and column commands (task 2.5a).
enum StructureText {
    static let insertRowAbove = String(localized: "Insert Row Above", comment: "Edit menu: insert an empty row above the selected cell's row (task 2.5a)")
    static let insertRowBelow = String(localized: "Insert Row Below", comment: "Edit menu: insert an empty row below the selected cell's row (⌘↩, task 2.5a)")
    static let duplicateRow = String(localized: "Duplicate Row", comment: "Edit menu (⇧⌘↩): a copy of the selected row below it (task 2.5a), and the Undo menu's Undo Duplicate Row")
    static let duplicateRows = String(localized: "Duplicate Rows", comment: "Edit menu with several rows selected (⇧⌘↩): a copy of each after the last, and the Undo menu's Undo Duplicate Rows")
    static let deleteRow = String(localized: "Delete Row", comment: "Edit menu (⌘⌫), and the Undo menu's Undo Delete Row")
    static let deleteRows = String(localized: "Delete Rows", comment: "Edit menu with several rows selected (⌘⌫), and the Undo menu's Undo Delete Rows")
    static let insertColumnBefore = String(localized: "Insert Column Before", comment: "Edit menu: insert an empty column before the selected cell's column (task 2.5a)")
    static let insertColumnAfter = String(localized: "Insert Column After", comment: "Edit menu: insert an empty column after the selected cell's column (task 2.5a)")
    static let deleteColumn = String(localized: "Delete Column", comment: "Edit menu, and the Undo menu's Undo Delete Column")
    static let deleteColumns = String(localized: "Delete Columns", comment: "Edit menu with several columns selected, and the Undo menu's Undo Delete Columns")

    /// Why a row or column command is off, for its tooltip, or `nil` if
    /// there is nothing to say (no column there to delete, say).
    static func reason(_ refusal: EditRefusal, _ command: StructureCommand) -> String? {
        switch refusal {
        case .stillReading:
            String(
                localized: "Rows and columns can be inserted and deleted once Leal has read the whole file.",
                comment: "Tooltip: Insert and Delete Row and Column are off until the file is read (ADR-0014 decision 1)"
            )
        case .saving:
            StatusText.waitForSave
        case .afterUnterminatedQuote:
            switch command {
            case .insertColumnBefore, .insertColumnAfter, .deleteColumns:
                String(
                    localized: "A quote in the last row is never closed, so a column inserted after it would land inside the quote. Insert Column Before still works, and the cell can still be edited.",
                    comment: "Tooltip: Insert Column is off after an unterminated quote (ADR-0004 decision 8)"
                )
            case .duplicateRows:
                String(
                    localized: "A quote in the last row is never closed, so a copy of the row would land inside the quote. Insert Row Above still works, and the cell can still be edited.",
                    comment: "Tooltip: Duplicate Row is off for an unterminated quote's row (ADR-0004 decision 8)"
                )
            case .insertRowAbove, .insertRowBelow, .deleteRows:
                String(
                    localized: "A quote in the last row is never closed, so a row inserted after it would land inside the quote. Insert Row Above still works, and the cell can still be edited.",
                    comment: "Tooltip: Insert Row is off after an unterminated quote (ADR-0004 decision 8)"
                )
            }
        case .tooManyRows:
            String(
                localized: "Duplicate up to \(Int(duplicateRowLimit()).formatted()) rows at a time.",
                comment: "Tooltip: Duplicate Row is off with more rows selected than it copies at once (task 2.5a); the limit"
            )
        case .noSuchRow, .notReadYet, .tooFarRight, .valueChanged, .otherLineage, .noSuchColumn, .unreadable:
            nil
        }
    }
}
