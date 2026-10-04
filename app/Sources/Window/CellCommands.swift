import AppKit
import LealFFI

/// Cut, Paste and Clear (task 2.6).
enum CellCommand: CaseIterable {
    /// Edit > Cut (⌘X): the selection copied, then its rows deleted (whole
    /// rows, picked by their numbers) or its cells emptied.
    case cut
    /// Edit > Paste (⌘V): the clipboard's cells into the selection.
    case paste
    /// Delete, Forward Delete or Edit > Delete: the selected cells emptied.
    case clear
}

/// Cut (⌘X), Paste (⌘V) and Clear (Delete) in the grid (task 2.6, DESIGN
/// §4.2): the Edit menu's own Cut, Paste and Delete items and the Delete
/// keys, no other UI (ADR-0002). Each commits an open edit first, as other
/// commands do, is one command of the core's, and goes through the
/// model's `commandApplied`, so it is one undo step ("Undo Cut", "Undo
/// Paste", "Undo Clear Cells"), in the recovery journal, and Find catches
/// up with it.
///
/// In the in-cell editor, the inspector, the find bar and Go to Row's
/// field, ⌘X, ⌘V and Delete are the text's: the grid isn't the first
/// responder then, so they never reach it.
///
/// The clipboard is read as tab-separated values (the core's `parse_tsv`),
/// the plain-text type if there is no tab-separated one: what Leal's Copy
/// and spreadsheets put there. One value goes into every selected cell; a
/// block goes in from the selection's top-left cell, which is then
/// selected. A line break inside a value becomes the file's own, as a
/// typed one does (`DocumentModel.lineEnding`). Empty text pastes nothing.
///
/// Cut is Copy, then Delete Row for whole rows picked by their numbers
/// (`GridSelection.wholeRows`), else Clear, as one step. It is refused as
/// the delete or the clear would be, and then copies nothing: the copy is
/// made first but goes on the clipboard only once the delete is done.
///
/// Refused while the file is read or saved (the menu item says why, as the
/// row commands do; the keys beep, and VoiceOver hears why); anything else
/// refused (a block that doesn't fit, too many cells or too much text, a
/// cell after an unterminated quote) is said in an alert, since the menu
/// can't show it before the clipboard is read.
extension DocumentViewController {
    /// The clipboard types Paste reads, in order.
    static let pasteTypes: [NSPasteboard.PasteboardType] = [.tabularText, .string]

    /// Whether `command` can run now, and if not, why.
    func availability(_ command: CellCommand) -> StructureAvailability {
        guard !model.isFailed, !isReplacingDocument, let selection = grid.selection,
              let area = model.cellArea(selection) else { return .off }
        let refusal: EditRefusal?
        switch command {
        case .cut:
            refusal = cutPlan().map(cutRefusal) ?? .unreadable
        case .paste:
            guard pasteboard.availableType(from: Self.pasteTypes) != nil else { return .off }
            refusal = model.pasteRefusal()
        case .clear:
            refusal = model.clearRefusal(area)
        }
        guard let refusal else { return .available }
        return StructureAvailability(enabled: false, reason: PasteText.reason(refusal, command))
    }

    /// The Edit menu's Cut, Paste and Delete, while the grid has the
    /// focus: on or off, with the reason as the tooltip.
    func validateCellItem(_ item: NSMenuItem) -> Bool {
        let command: CellCommand = switch item.action {
        case #selector(GridView.cut(_:)): .cut
        case #selector(GridView.paste(_:)): .paste
        default: .clear
        }
        let state = availability(command)
        item.toolTip = state.reason
        return state.enabled
    }

    /// ⌘V in the grid.
    func pasteIntoSelection() {
        run(.paste)
    }

    /// Delete in the grid.
    func clearSelection() {
        run(.clear)
    }

    /// The rows Cut deletes: those of whole rows picked by their numbers,
    /// within the file (to the last row if the selection runs there, as
    /// `cellArea` does); `nil` for cells.
    private func rowsToCut(_ selection: GridSelection) -> ClosedRange<Int>? {
        guard selection.wholeRows, model.rowCount > 0, selection.rows.lowerBound < model.rowCount else { return nil }
        let last = selection.throughLastRow ? model.rowCount - 1 : min(selection.rows.upperBound, model.rowCount - 1)
        return selection.rows.lowerBound...last
    }

    /// What Cut would do now: the selection, its cells, the rows it deletes
    /// (`nil` for cells) and what it copies. Whole rows are deleted whole,
    /// so they are copied whole: every column the file or the grid has,
    /// whatever the selection's columns were when the rows were picked
    /// (the grid may have widened since), and the rows deleted, no more.
    struct CutPlan: Equatable {
        let selection: GridSelection
        let area: CellArea
        let rows: ClosedRange<Int>?
        let range: CopyRange
    }

    private func cutPlan() -> CutPlan? {
        guard let selection = grid.selection, let area = model.cellArea(selection) else { return nil }
        let rows = rowsToCut(selection)
        var range = model.copyRange(selection)
        if let rows {
            range = CopyRange(
                rowStart: UInt64(rows.lowerBound + model.headerRows),
                rowCount: UInt64(rows.count),
                columnStart: 0,
                columnCount: UInt32(clamping: max(model.columnCount, model.fileColumnCount, 1))
            )
        }
        return CutPlan(selection: selection, area: area, rows: rows, range: range)
    }

    /// Why Cut can't delete or clear `plan` now: as Delete Row or Clear
    /// would refuse.
    private func cutRefusal(_ plan: CutPlan) -> EditRefusal? {
        plan.rows != nil ? model.rowDeleteRefusal() : model.clearRefusal(plan.area)
    }

    /// Runs `command`, after committing an open edit.
    private func run(_ command: CellCommand) {
        scheduler.noteUserInput()
        guard !model.isFailed, !isReplacingDocument, commitEditing(),
              let selection = grid.selection, let area = model.cellArea(selection) else { return NSSound.beep() }
        // Asked before the clipboard is read: it may hold a lot.
        if let refusal = command == .paste ? model.pasteRefusal() : model.clearRefusal(area) {
            return refused(command, refusal)
        }
        let text = command == .paste ? Self.pastedText(pasteboard) : nil
        if command == .paste {
            // Nothing to paste, or empty text: nothing changes (Rob,
            // 2026-10-04), rather than the selection being cleared.
            guard let text, !text.isEmpty else { return NSSound.beep() }
            // Too much to hand the core: it would only be copied to say so.
            if text.utf8.count > Int(clamping: pasteByteLimit()) { return refused(.paste, .tooMuchText) }
        }
        // The user chose where to work: a Next still waiting for the search
        // doesn't move the selection now.
        find.cancelPendingStep()
        var outcome = EditOutcome.unchanged
        if let text {
            var shape: (rows: UInt64, columns: UInt32)?
            asOneStep {
                (outcome, shape) = model.paste(text, into: area)
                return PasteText.paste
            }
            if case .edited = outcome, let shape { selectPasted(shape, at: selection) }
        } else {
            asOneStep {
                outcome = model.clear(area)
                return PasteText.clearCells
            }
        }
        if case let .refused(refusal) = outcome { refused(command, refusal) }
    }

    /// The clipboard's text, tab-separated first.
    static func pastedText(_ pasteboard: NSPasteboard) -> String? {
        for type in pasteTypes {
            if let text = pasteboard.string(forType: type) { return text }
        }
        return nil
    }

    /// After a block is pasted, its cells are selected, from the
    /// selection's top-left cell (one value pasted into the selection
    /// leaves it as it was).
    private func selectPasted(_ shape: (rows: UInt64, columns: UInt32), at selection: GridSelection) {
        guard shape.rows > 1 || shape.columns > 1 else { return }
        let corner = CellPosition(row: selection.rows.lowerBound, column: selection.columns.lowerBound)
        let extent = CellPosition(row: corner.row + Int(shape.rows) - 1, column: corner.column + Int(shape.columns) - 1)
        grid.select(GridSelection(active: corner, anchor: corner, extent: extent))
    }

    // MARK: Cut

    /// ⌘X in the grid: the selection copied as Copy copies it, then its
    /// rows deleted (whole rows) or its cells cleared, as one step named
    /// Cut. Refused as the delete or clear would be, copying nothing. A
    /// copy of over about 100 MB (whole rows of a large file) asks first,
    /// as Copy does.
    func cutSelection() {
        scheduler.noteUserInput()
        guard !model.isFailed, !isReplacingDocument, commitEditing(), let plan = cutPlan() else { return NSSound.beep() }
        if let refusal = cutRefusal(plan) { return refused(.cut, refusal) }
        let bytes = model.estimatedCopyBytes(plan.range)
        guard bytes > askBeforeCopyBytes else { return cut(plan, bytes: bytes) }
        confirmLargeCopy(bytes) { [weak self] go in
            guard go, let self else { return }
            // The sheet was up a while: the selection, the rows or the
            // columns may have changed (the file read again, say), and
            // what was asked about is no longer what would be cut.
            guard !model.isFailed, !isReplacingDocument, cutPlan() == plan, cutRefusal(plan) == nil else {
                return NSSound.beep()
            }
            cut(plan, bytes: bytes)
        }
    }

    private func cut(_ plan: CutPlan, bytes: UInt64) {
        find.cancelPendingStep()
        // Made before the cells change (a job copies the edits as they are
        // when it starts), placed only once they have.
        guard let copy = prepareCopy(plan.range, bytes: bytes) else { return NSSound.beep() }
        let (rows, area) = (plan.rows, plan.area)
        var outcome = EditOutcome.unchanged
        asOneStep {
            outcome = if let rows { model.deleteRows(rows) } else { model.clear(area) }
            return PasteText.cut
        }
        switch outcome {
        case .edited, .unchanged:
            place(copy)
        case let .refused(refusal):
            discard(copy)
            refused(.cut, refusal, rows: rows != nil)
        case .failed:
            discard(copy)
        }
    }

    /// Says why `command` didn't change anything: for a reason the user
    /// waits out (the file is read or saved) a beep, and VoiceOver hears
    /// it, as for the row commands; otherwise in an alert on the window.
    /// `rows`: a Cut of whole rows, refused as Delete Row is.
    private func refused(_ command: CellCommand, _ refusal: EditRefusal, rows: Bool = false) {
        NSSound.beep()
        let reason = rows && refusal == .afterUnterminatedQuote
            ? StructureText.reason(refusal, .deleteRows)
            : PasteText.reason(refusal, command)
        guard let reason else { return }
        if refusal == .stillReading || refusal == .saving {
            announce(reason)
            return
        }
        let alert = NSAlert()
        alert.messageText = switch command {
        case .cut: PasteText.cantCut
        case .paste: PasteText.cantPaste
        case .clear: PasteText.cantClear
        }
        alert.informativeText = reason
        present(alert)
    }
}

/// The words of Cut, Paste and Clear (task 2.6).
enum PasteText {
    static let cut = String(localized: "Cut", comment: "The Undo menu's Undo Cut: Cut copied the selection, then deleted its rows or emptied its cells (task 2.6)")
    static let paste = String(localized: "Paste", comment: "The Undo menu's Undo Paste (task 2.6)")
    static let clearCells = String(localized: "Clear Cells", comment: "The Undo menu's Undo Clear Cells: Delete emptied the selected cells (task 2.6)")
    static let cantPaste = String(localized: "Leal can’t paste these cells here.", comment: "Alert title: a paste was refused (task 2.6)")
    static let cantClear = String(localized: "Leal can’t clear these cells.", comment: "Alert title: Delete couldn't clear the selected cells (task 2.6)")
    static let cantCut = String(localized: "Leal can’t cut these cells.", comment: "Alert title: Cut couldn't delete the selected rows or clear the selected cells, so nothing was copied (task 2.6)")

    /// Why Cut, Paste or Clear didn't, or can't, run: the menu item's tooltip,
    /// or the alert's text. `nil` if there is nothing to say.
    static func reason(_ refusal: EditRefusal, _ command: CellCommand) -> String? {
        switch (refusal, command) {
        case (.stillReading, .paste):
            String(localized: "Cells can be pasted into once Leal has read the whole file.", comment: "Tooltip: Paste is off until the file is read (task 2.6)")
        case (.stillReading, .clear):
            String(localized: "Cells can be cleared once Leal has read the whole file.", comment: "Tooltip: Delete is off until the file is read (task 2.6)")
        case (.stillReading, .cut):
            String(localized: "Cells and rows can be cut once Leal has read the whole file.", comment: "Tooltip: Cut is off until the file is read (task 2.6)")
        case (.saving, _):
            StatusText.waitForSave
        case (.tooManyCells, .paste):
            String(
                localized: "Leal pastes up to \(Int(cellBatchLimit()).formatted()) cells at a time.",
                comment: "Alert: a paste of too many cells was refused (task 2.6); the limit"
            )
        case (.tooManyCells, .clear):
            String(
                localized: "Leal clears up to \(Int(cellBatchLimit()).formatted()) cells at a time. To empty a whole column, delete it and insert an empty one.",
                comment: "Tooltip and alert: Delete is off with more cells selected than it clears at once (task 2.6); the limit"
            )
        case (.tooManyCells, .cut):
            String(
                localized: "Leal cuts up to \(Int(cellBatchLimit()).formatted()) cells at a time. To cut whole rows, select them by their row numbers.",
                comment: "Tooltip and alert: Cut is off with more cells selected than it clears at once (task 2.6); the limit"
            )
        case (.tooMuchReplaced, .paste):
            String(
                localized: "The cells pasted into hold more than \(ByteCountFormatter.string(fromByteCount: Int64(pasteByteLimit()), countStyle: .file)) of text, more than Leal replaces at a time. Paste into fewer cells.",
                comment: "Alert: a paste over cells holding too much text was refused (task 2.6); the limit, as in 32 MB"
            )
        case (.tooMuchReplaced, _):
            String(
                localized: "The selected cells hold more than \(ByteCountFormatter.string(fromByteCount: Int64(pasteByteLimit()), countStyle: .file)) of text, more than Leal clears at a time. To empty a whole column, delete it and insert an empty one.",
                comment: "Alert: Delete or Cut of cells holding too much text was refused (task 2.6); the limit, as in 32 MB"
            )
        case (.tooMuchText, _):
            String(
                localized: "Leal pastes up to \(ByteCountFormatter.string(fromByteCount: Int64(pasteByteLimit()), countStyle: .file)) of text at a time.",
                comment: "Alert: a paste of too much text was refused (task 2.6); the limit, as in 32 MB"
            )
        case (.pastLastRow, _):
            String(
                localized: "The copied cells run past the last row. Paste them higher up, or insert rows first.",
                comment: "Alert: a block of cells pasted would run past the file's last row (task 2.6)"
            )
        case (.pastLastColumn, _):
            String(
                localized: "The copied cells run past the last column. Paste them further left, or insert columns first.",
                comment: "Alert: a block of cells pasted would run past the last column (task 2.6)"
            )
        case (.afterUnterminatedQuote, _):
            String(
                localized: "A quote earlier in a row is never closed, so the cells after it are inside the quote and can’t be changed.",
                comment: "Alert: a paste or clear reached cells after an unterminated quote (ADR-0004 decision 8, task 2.6)"
            )
        case (.noSuchRow, _), (.noSuchColumn, _), (.otherLineage, _), (.tooManyRows, _):
            nil
        case (.notReadYet, _), (.tooFarRight, _), (.unreadable, _), (.valueChanged, _):
            EditText.refusal(refusal)
        }
    }
}
