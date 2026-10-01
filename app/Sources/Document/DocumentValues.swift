import AppKit
import LealFFI
import os

/// The core's rows and fields of a copy of a selection.
struct CopyRange: Equatable, Sendable {
    let rowStart: UInt64
    let rowCount: UInt64
    let columnStart: UInt32
    let columnCount: UInt32
}

/// Whole values from the core (task 1.8): Copy and the cell inspector.
/// What is copied and shown, and how it is counted, is the core's
/// (`Document.copyCells`, `Document.cellValue`); this turns grid positions
/// into the core's rows and runs the calls where they belong.
extension DocumentModel {
    /// The most characters of a value the inspector shows. An `NSTextView`
    /// lays out a long line of 64,000 characters in a few milliseconds even
    /// on an M1 Air; a million took 800 ms on an M5 Pro (1.8 review). A
    /// longer value says it is cut.
    nonisolated static let inspectorMaxCharacters: UInt32 = 64_000

    /// The core's rows and fields for `selection`. A selection that runs to
    /// the last row while indexing (⌘A) runs to the end of the file,
    /// however many rows it turns out to have.
    func copyRange(_ selection: GridSelection) -> CopyRange {
        CopyRange(
            rowStart: UInt64(selection.rows.lowerBound + headerRows),
            rowCount: selection.throughLastRow && !isIndexComplete ? UInt64(Int.max) : UInt64(selection.rows.count),
            columnStart: UInt32(clamping: selection.columns.lowerBound),
            columnCount: UInt32(clamping: selection.columns.count)
        )
    }

    /// About how many bytes copying `range` gives, without reading it.
    func estimatedCopyBytes(_ range: CopyRange) -> UInt64 {
        call {
            try $0.estimatedCopyBytes(rowStart: range.rowStart, rowCount: range.rowCount, columnStart: range.columnStart, columnCount: range.columnCount)
        } ?? 0
    }

    /// The text of `range` at once, if the index has all of it.
    func copyCellsNow(_ range: CopyRange) -> String? {
        call {
            try $0.copyCellsNow(rowStart: range.rowStart, rowCount: range.rowCount, columnStart: range.columnStart, columnCount: range.columnCount)
        } ?? nil
    }

    /// Starts copying `range` as tab-separated text, a P2 job in the core.
    func copyCells(_ range: CopyRange) -> CopyJob? {
        call {
            try $0.copyCells(rowStart: range.rowStart, rowCount: range.rowCount, columnStart: range.columnStart, columnCount: range.columnCount)
        }
    }

    /// Grid row `row`, column `column`'s whole value, for the inspector: off
    /// the main actor, since a value of many megabytes takes a while to
    /// decode and count. `nil` if the row isn't read yet, the file was read
    /// again meanwhile, or the call failed.
    func cellValue(row: Int, column: Int) async -> CellValue? {
        guard let handle = backgroundHandle() else { return nil }
        let physical = UInt64(row + headerRows)
        let generation = generation
        let result = await Task.detached(priority: .userInitiated) { () -> Result<CellValue?, any Error> in
            Result {
                try handle.cellValue(row: physical, column: UInt32(clamping: column), maxChars: Self.inspectorMaxCharacters)
            }
        }.value
        guard generation == self.generation, !isFailed else { return nil }
        switch result {
        case let .success(value):
            return value
        case let .failure(error):
            // As any core call's error: a failure fails the document.
            let _: Void? = call { _ -> Void in throw error }
            return nil
        }
    }
}

/// Puts copied cells on a pasteboard (DESIGN §4.2: TSV on the clipboard),
/// as tab-separated text and as plain text, so spreadsheets paste a table
/// and text fields paste the text. The two types share one copy of the
/// bytes.
enum Clipboard {
    static let types: [NSPasteboard.PasteboardType] = [.tabularText, .string]

    /// `text`, now.
    static func write(_ text: String, to pasteboard: NSPasteboard) {
        let data = Data(text.utf8)
        pasteboard.clearContents()
        pasteboard.declareTypes(types, owner: nil)
        for type in types {
            pasteboard.setData(data, forType: type)
        }
    }

    /// `job`'s text, promised now and given when something pastes it
    /// (`CopyPromise`). So a paste straight after ⌘C never gets the old
    /// clipboard, and the text, which may be large, isn't copied into the
    /// pasteboard unless it is used.
    static func promise(_ job: CopyJob, to pasteboard: NSPasteboard) -> CopyPromise {
        let promise = CopyPromise(job: job)
        pasteboard.clearContents()
        let item = NSPasteboardItem()
        item.setDataProvider(promise, forTypes: types)
        pasteboard.writeObjects([item])
        return promise
    }
}

/// The text of a copy that is still running, promised to a pasteboard. The
/// pasteboard asks for it, on the main thread, when something pastes; if
/// the copy hasn't finished, that waits for it there (a paste must be
/// answered at once). The text is taken from the core then, once, and
/// given for both types.
///
/// **It outlives its window.** Closing the document, reading the file
/// again or reloading it doesn't stop the copy: the core's job keeps its
/// own reading and the file's clone, so it still gives exactly the cells
/// that were copied (1.8 re-review). AppKit asks for promised data when
/// the app quits, after the windows have closed, so that works too. Only
/// the pasteboard letting go (`pasteboardFinishedWithDataProvider`: a new
/// copy here, or another app's) stops it.
///
/// Once both types have been given, the text and the core's job are let
/// go, and with them the clone, if the document has closed. If the copy
/// failed (its removable drive vanished), the pasteboard gets nothing, not
/// an empty string, and the failure is logged.
///
/// `@unchecked Sendable`: its state is behind a lock, and the `CopyJob` is
/// `Sendable`. AppKit calls it on the main thread.
final class CopyPromise: NSObject, NSPasteboardItemDataProvider, @unchecked Sendable {
    /// How long a paste waits for a copy still running.
    static let waitMilliseconds: UInt32 = 30_000

    private let lock = NSLock()
    /// The core's job, until its text has been taken.
    private var pending: CopyJob?
    /// The text, from when it is taken until both types have been given.
    private var data: Data?
    private var given: Set<NSPasteboard.PasteboardType> = []
    /// Promises some pasteboard may still ask for, or tell that it is
    /// finished with: a pasteboard item doesn't promise to keep its data
    /// provider alive, so they are kept here until it does.
    nonisolated(unsafe) private static var outstanding: [ObjectIdentifier: CopyPromise] = [:]
    private static let outstandingLock = NSLock()

    init(job: CopyJob) {
        pending = job
        super.init()
        Self.outstandingLock.withLock { Self.outstanding[ObjectIdentifier(self)] = self }
    }

    /// The core's job, until its text has been taken (for tests).
    var job: CopyJob? { lock.withLock { pending } }

    /// Whether it still holds the text or the job (for tests).
    var isHoldingText: Bool { lock.withLock { pending != nil || data != nil } }

    /// Promises not yet let go of by their pasteboard (for tests).
    static var outstandingCount: Int { outstandingLock.withLock { outstanding.count } }

    func pasteboard(_ pasteboard: NSPasteboard?, item: NSPasteboardItem, provideDataForType type: NSPasteboard.PasteboardType) {
        let data: Data? = lock.withLock {
            if data == nil, let job = pending {
                pending = nil
                do {
                    if let text = try job.takeWaiting(timeoutMs: Self.waitMilliseconds) {
                        data = Data(text.utf8)
                    } else {
                        Logger.document.error("A copy promised to the pasteboard had no text (it failed or was stopped); the paste gets nothing")
                    }
                } catch {
                    Logger.document.error("A copy promised to the pasteboard failed: \(String(describing: error), privacy: .public)")
                }
            }
            let shown = data
            given.insert(type)
            if given.isSuperset(of: Clipboard.types) {
                // Every type has its bytes now; the pasteboard keeps them.
                self.data = nil
            }
            return shown
        }
        if let data { item.setData(data, forType: type) }
    }

    func pasteboardFinishedWithDataProvider(_ pasteboard: NSPasteboard) {
        let job = lock.withLock {
            defer { pending = nil; data = nil }
            return pending
        }
        // Nothing pasted it: the copy, if still running, isn't wanted.
        job?.cancel()
        _ = Self.outstandingLock.withLock { Self.outstanding.removeValue(forKey: ObjectIdentifier(self)) }
    }
}
