import AppKit
import LealFFI

/// Paste and Clear (task 2.6).
enum CellCommand: CaseIterable {
    /// Edit > Paste (⌘V): the clipboard's cells into the selection.
    case paste
    /// Delete, Forward Delete or Edit > Delete: the selected cells emptied.
    case clear
}

/// Paste (⌘V) and Clear (Delete) in the grid (task 2.6, DESIGN §4.2): the
/// Edit menu's own Paste and Delete items and the Delete keys, no other UI
/// (ADR-0002). Each commits an open edit first, as other commands do, is
/// one command of the core's, and goes through the model's
/// `commandApplied`, so it is one undo step ("Undo Paste", "Undo Clear
/// Cells"), in the recovery journal, and Find catches up with it.
///
/// In the in-cell editor, the inspector and the find bar, ⌘V and Delete
/// are the text's: the grid isn't the first responder then, so they never
/// reach it.
///
/// The clipboard is read as tab-separated values (the core's `parse_tsv`),
/// the plain-text type if there is no tab-separated one: what Leal's Copy
/// and spreadsheets put there. One value goes into every selected cell; a
/// block goes in from the selection's top-left cell, which is then
/// selected.
///
/// Refused while the file is read or saved (the menu item says why, as the
/// row commands do; the keys beep, and VoiceOver hears why); anything else
/// refused (a block that doesn't fit, too many cells, a cell after an
/// unterminated quote) is said in an alert, since the menu can't show it
/// before the clipboard is read.
extension DocumentViewController {
    /// The clipboard types Paste reads, in order.
    static let pasteTypes: [NSPasteboard.PasteboardType] = [.tabularText, .string]

    /// Whether `command` can run now, and if not, why.
    func availability(_ command: CellCommand) -> StructureAvailability {
        guard !model.isFailed, !isReplacingDocument, let selection = grid.selection,
              let area = model.cellArea(selection) else { return .off }
        let refusal: EditRefusal?
        switch command {
        case .paste:
            guard pasteboard.availableType(from: Self.pasteTypes) != nil else { return .off }
            refusal = model.pasteRefusal()
        case .clear:
            refusal = model.clearRefusal(area)
        }
        guard let refusal else { return .available }
        return StructureAvailability(enabled: false, reason: PasteText.reason(refusal, command))
    }

    /// The Edit menu's Paste and Delete, while the grid has the focus: on
    /// or off, with the reason as the tooltip.
    func validateCellItem(_ item: NSMenuItem) -> Bool {
        let command: CellCommand = item.action == #selector(GridView.paste(_:)) ? .paste : .clear
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

    /// Runs `command`, after committing an open edit.
    private func run(_ command: CellCommand) {
        scheduler.noteUserInput()
        guard !model.isFailed, !isReplacingDocument, commitEditing(),
              let selection = grid.selection, let area = model.cellArea(selection) else { return NSSound.beep() }
        let text = command == .paste ? Self.pastedText(pasteboard) : nil
        if command == .paste, text == nil { return NSSound.beep() }
        if let refusal = command == .paste ? model.pasteRefusal() : model.clearRefusal(area) {
            return refused(command, refusal)
        }
        // The user chose where to work: a Next still waiting for the search
        // doesn't move the selection now.
        find.cancelPendingStep()
        var outcome = EditOutcome.unchanged
        if let text {
            asOneStep {
                outcome = model.paste(text, into: area)
                return PasteText.paste
            }
            if case .edited = outcome { selectPasted(text, at: selection) }
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
    private func selectPasted(_ text: String, at selection: GridSelection) {
        guard let shape = pasteShape(text: text), shape.rows > 1 || shape.columns > 1 else { return }
        let corner = CellPosition(row: selection.rows.lowerBound, column: selection.columns.lowerBound)
        let extent = CellPosition(row: corner.row + Int(shape.rows) - 1, column: corner.column + Int(shape.columns) - 1)
        grid.select(GridSelection(active: corner, anchor: corner, extent: extent))
    }

    /// Says why `command` didn't change anything: for a reason the user
    /// waits out (the file is read or saved) a beep, and VoiceOver hears
    /// it, as for the row commands; otherwise in an alert on the window.
    private func refused(_ command: CellCommand, _ refusal: EditRefusal) {
        NSSound.beep()
        guard let reason = PasteText.reason(refusal, command) else { return }
        if refusal == .stillReading || refusal == .saving {
            announce(reason)
            return
        }
        let alert = NSAlert()
        alert.messageText = command == .paste ? PasteText.cantPaste : PasteText.cantClear
        alert.informativeText = reason
        present(alert)
    }
}

/// The words of Paste and Clear (task 2.6).
enum PasteText {
    static let paste = String(localized: "Paste", comment: "The Undo menu's Undo Paste (task 2.6)")
    static let clearCells = String(localized: "Clear Cells", comment: "The Undo menu's Undo Clear Cells: Delete emptied the selected cells (task 2.6)")
    static let cantPaste = String(localized: "Leal can’t paste these cells here.", comment: "Alert title: a paste was refused (task 2.6)")
    static let cantClear = String(localized: "Leal can’t clear these cells.", comment: "Alert title: Delete couldn't clear the selected cells (task 2.6)")

    /// Why Paste or Clear didn't, or can't, run: the menu item's tooltip,
    /// or the alert's text. `nil` if there is nothing to say.
    static func reason(_ refusal: EditRefusal, _ command: CellCommand) -> String? {
        switch (refusal, command) {
        case (.stillReading, .paste):
            String(localized: "Cells can be pasted into once Leal has read the whole file.", comment: "Tooltip: Paste is off until the file is read (task 2.6)")
        case (.stillReading, .clear):
            String(localized: "Cells can be cleared once Leal has read the whole file.", comment: "Tooltip: Delete is off until the file is read (task 2.6)")
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
