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
    /// nothing. Nothing was written either way.
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
            let first = cells.first.map { cell($0, headerRows: headerRows) } ?? ""
            if cells.count == 1 && !more {
                detail = String(
                    localized: "The cell at \(first) holds bytes that aren’t \(name) text, so they can’t be converted. Leal never replaces them.",
                    comment: "Alert text: Save As UTF-8 refused one cell; where it is, as in \"row 3, column 2\", and the file's encoding"
                )
            } else {
                let count = more ? String(localized: "More than \(cells.count.formatted())", comment: "Alert text: how many cells Save As UTF-8 refused, when it stopped counting") : cells.count.formatted()
                detail = String(
                    localized: "\(count) cells hold bytes that aren’t \(name) text, the first at \(first), so they can’t be converted. Leal never replaces them.",
                    comment: "Alert text: Save As UTF-8 refused several cells; how many, the file's encoding, and where the first is"
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
        case let .Io(_, _, message):
            detail = message
        default:
            detail = String(
                localized: "Leal couldn’t read all of the file. Try again, or reopen it first.",
                comment: "Alert text: Save As UTF-8 failed for another reason"
            )
        }
        return Message(title: title, detail: detail)
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
