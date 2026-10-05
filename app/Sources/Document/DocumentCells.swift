import Foundation
import LealFFI

/// The cells of a selection, as the core takes them: logical rows (after
/// the header row) and columns.
struct CellArea: Equatable, Sendable {
    let rowStart: UInt64
    let rowCount: UInt64
    let columnStart: UInt32
    let columnCount: UInt32

    /// How many cells it has.
    var cells: UInt64 { rowCount * UInt64(columnCount) }
}

/// Paste and Clear (task 2.6, DESIGN §4.2; Cut is Copy then Clear or
/// Delete Row): the core's `paste` and
/// `clearCells`, with the grid's selection turned into the core's logical
/// rows. Each is one command of its cells, which goes to `commandApplied`
/// as a cell edit does, so undo, the journal, the grid, Find and the
/// inspector hear of it there.
///
/// The core decides whether one is allowed (not until the whole file is
/// read, nor while a save runs, as for rows and columns; at most
/// `cellBatchLimit()` cells; a block must fit; nothing after an
/// unterminated quote) and how the clipboard's text is split into cells.
extension DocumentModel {
    /// The cells of `selection` within the file's rows and the grid's
    /// columns, or `nil` if it has none. A selection that runs to the last
    /// row (⌘A) runs to the last row there is now.
    func cellArea(_ selection: GridSelection) -> CellArea? {
        let rows = rowCount
        let columns = columnCount
        guard rows > 0, columns > 0, selection.rows.lowerBound < rows, selection.columns.lowerBound < columns else { return nil }
        let lastRow = selection.throughLastRow ? rows - 1 : min(selection.rows.upperBound, rows - 1)
        let lastColumn = min(selection.columns.upperBound, columns - 1)
        return CellArea(
            rowStart: UInt64(selection.rows.lowerBound + headerRows),
            rowCount: UInt64(lastRow - selection.rows.lowerBound + 1),
            columnStart: UInt32(clamping: selection.columns.lowerBound),
            columnCount: UInt32(clamping: lastColumn - selection.columns.lowerBound + 1)
        )
    }

    /// Why Paste can't run now (the file is still being read, a save
    /// runs), or `nil` if it can. What is pasted is checked as it is
    /// pasted.
    func pasteRefusal() -> EditRefusal? {
        let refusal: EditRefusal?? = call { try $0.canPaste() }
        return refusal ?? .unreadable
    }

    /// Why `area` can't be cleared now, or `nil` if it can.
    func clearRefusal(_ area: CellArea) -> EditRefusal? {
        let refusal: EditRefusal?? = call {
            try $0.canClearCells(rowStart: area.rowStart, rowCount: area.rowCount, columnStart: area.columnStart, columnCount: area.columnCount)
        }
        return refusal ?? .unreadable
    }

    /// Pastes clipboard `text` into `area`: one value into each cell, or a
    /// block from its top-left cell, which must fit within the rows and the
    /// grid's columns. A line break inside a value becomes the file's own,
    /// as one typed with ⌥↩ does (`lineBreak`). Also the size of what was
    /// pasted, to select the block (`nil` if the call didn't run).
    func paste(_ text: String, into area: CellArea) -> (EditOutcome, (rows: UInt64, columns: UInt32)?) {
        let shown = UInt32(clamping: columnCount)
        let ending = lineEnding ?? .lf
        var shape: (rows: UInt64, columns: UInt32)?
        let outcome = make {
            let pasting = try $0.paste(
                rowStart: area.rowStart, rowCount: area.rowCount, columnStart: area.columnStart, columnCount: area.columnCount,
                columnsShown: shown, text: text, lineEnding: ending
            )
            shape = (pasting.rows, pasting.columns)
            return pasting.command
        }
        return (outcome, shape)
    }

    /// Clears `area`: each cell empty, a missing (hatched) one left missing.
    func clear(_ area: CellArea) -> EditOutcome {
        make {
            try $0.clearCells(rowStart: area.rowStart, rowCount: area.rowCount, columnStart: area.columnStart, columnCount: area.columnCount)
        }
    }
}
