import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.6 against the real core, hosted in Leal.app: Paste (⌘V) of one
/// value or a block, and Clear (Delete), each one undo step, in the
/// journal; the header row; hatched cells (ADR-0005 decision 2); the
/// refusals (a block past the last row or column, too many cells, after an
/// unterminated quote, while reading or saving); Delete left to the
/// editors; Find catching up; the bytes saved; and what 100,000 cells
/// cost the main thread.
///
/// No sheet is shown (`showAlert` and `CSVDocument.showSheet` are
/// replaced), the clipboard is a private pasteboard, and nothing is sent
/// to the system: key events are made here and handed to the views
/// (CLAUDE.md).
@MainActor
final class PasteTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?
    private var pasteboard: NSPasteboard!

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-paste-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        environment = DocumentEnvironment(
            scheduler: try Scheduler(),
            temp: TempLocations(
                scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
                recordsDir: directory.appending(path: "records").path(percentEncoded: false)
            )
        )
        savedEnvironment = CSVDocument.environment
        let environment = environment!
        CSVDocument.environment = { environment }
        pasteboard = NSPasteboard(name: NSPasteboard.Name("io.github.robhaswell.leal.tests.paste-\(UUID().uuidString)"))
    }

    override func tearDown() async throws {
        debugReleaseHeldSave()
        DocumentModel.openForTesting = nil
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        pasteboard.releaseGlobally()
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private let csv = "id,name,qty\r\n1,Marlow,3\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n"

    private func file(_ name: String, _ text: String) throws -> URL {
        let url = directory.appending(path: name)
        try Data(text.utf8).write(to: url)
        return url
    }

    private struct Opened {
        let document: CSVDocument
        let model: DocumentModel
        let content: DocumentViewController
        let window: NSWindow
        @MainActor var undo: DocumentUndoManager { document.history.undoManager }
        @MainActor var grid: GridContainerView { content.grid }
    }

    /// The alerts shown, by their text.
    private var alerts: [String] = []

    private func open(_ url: URL) async throws -> Opened {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        addTeardownBlock { @MainActor in withExtendedLifetime(document) {} }
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete && model.pasteRefusal() == nil }
        document.isOnScreen = { _ in true }
        document.showSheet = { alert, _, done in
            XCTFail("unexpected sheet: \(alert.messageText)")
            done(NSApplication.ModalResponse(rawValue: NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + alert.buttons.count - 1))
        }
        content.pasteboard = pasteboard
        content.showAlert = { [weak self] alert, _ in self?.alerts.append(alert.informativeText) }
        return Opened(document: document, model: model, content: content, window: window)
    }

    private func waitUntil(_ what: String, timeout: TimeInterval = 30, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting until \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    private func copy(_ text: String) {
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }

    private func value(_ model: DocumentModel, _ row: Int, _ column: Int) -> String? {
        model.fullValue(.cell(CellPosition(row: row, column: column)))
    }

    private func row(_ model: DocumentModel, _ row: Int) -> [String?] {
        (0..<model.columnCount).map { value(model, row, $0) }
    }

    private func select(_ opened: Opened, _ from: (Int, Int), _ to: (Int, Int)) {
        let anchor = CellPosition(row: from.0, column: from.1)
        opened.grid.select(GridSelection(active: anchor, anchor: anchor, extent: CellPosition(row: to.0, column: to.1)))
    }

    private func item(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }

    /// Whether Edit > Paste or Delete is on in the grid, and its tooltip.
    private func validate(_ opened: Opened, _ action: Selector) -> (Bool, String?) {
        let item = item(action)
        let enabled = opened.grid.gridView.validateMenuItem(item)
        return (enabled, item.toolTip)
    }

    private func save(_ opened: Opened) async throws -> Data {
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        let saved = await saving.value
        XCTAssertTrue(saved, "saved")
        return try Data(contentsOf: try XCTUnwrap(opened.document.fileURL))
    }

    /// ⌫ (or ⌦) as the keyboard gives it, with no modifier.
    private func deleteKey(_ window: NSWindow, forward: Bool = false, repeating: Bool = false) throws -> NSEvent {
        let characters = forward ? "\u{F728}" : "\u{7f}"
        return try XCTUnwrap(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: forward ? .function : [], timestamp: 0, windowNumber: window.windowNumber,
            context: nil, characters: characters, charactersIgnoringModifiers: characters, isARepeat: repeating, keyCode: forward ? 117 : 51
        ))
    }

    // MARK: Paste

    /// A block goes in from the selection's top-left cell (the header row
    /// is never a grid row), as one undo step named Paste, in the journal;
    /// its cells are selected; undo and redo put them back and again.
    func testABlockPastesFromTheTopLeftCellAsOneStep() async throws {
        let opened = try await open(file("block.csv", csv))
        let (model, content) = (opened.model, opened.content)
        select(opened, (0, 1), (2, 2))
        copy("A\tB\r\nC\tD\r\n")
        XCTAssertTrue(validate(opened, #selector(GridView.paste(_:))).0)
        let journal = opened.document.history.journal.count
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(row(model, 0), ["1", "A", "B"])
        XCTAssertEqual(row(model, 1), ["2", "C", "D"])
        XCTAssertEqual(row(model, 2), ["3", "Halden", "8"], "the block once, not over the whole selection")
        XCTAssertEqual(model.headerTitle(column: 1).text, "name")
        XCTAssertEqual(opened.grid.selection, GridSelection(
            active: CellPosition(row: 0, column: 1), anchor: CellPosition(row: 0, column: 1), extent: CellPosition(row: 1, column: 2)
        ))
        XCTAssertEqual(opened.document.history.journal.count, journal + 1, "one command")
        XCTAssertEqual(opened.undo.undoActionName, "Paste")
        XCTAssertTrue(model.hasUnsavedEdits)
        opened.undo.undo()
        XCTAssertEqual(row(model, 0), ["1", "Marlow", "3"])
        XCTAssertEqual(row(model, 1), ["2", "Ostrava", "5"])
        XCTAssertFalse(model.hasUnsavedEdits)
        XCTAssertEqual(opened.undo.redoActionName, "Paste")
        opened.undo.redo()
        XCTAssertEqual(row(model, 1), ["2", "C", "D"])
        XCTAssertTrue(alerts.isEmpty)
        _ = content
    }

    /// One value goes into every selected cell; the selection stays. In a
    /// short row's missing (hatched) cells it is typed in as an edit would
    /// be; `""` there is no edit (ADR-0005 decision 2), and Delete leaves
    /// them missing.
    func testOneValueFillsTheSelectionAndHatchedCellsFollowTheRule() async throws {
        let opened = try await open(file("fill.csv", "a,b,c\n1,2,3\n4\n5,6,7\n"))
        let model = opened.model
        try await waitUntil("the short row known") { model.isHatched(row: 1, column: 1) }
        select(opened, (0, 1), (1, 2))
        copy("x\n")
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(row(model, 0), ["1", "x", "x"])
        XCTAssertEqual(row(model, 1), ["4", "x", "x"], "the short row padded")
        XCTAssertEqual(opened.grid.selection?.extent, CellPosition(row: 1, column: 2), "the selection stays")
        opened.undo.undo()
        XCTAssertFalse(model.hasUnsavedEdits)

        // "" into the hatched cells: no edit, no undo step.
        select(opened, (1, 1), (1, 2))
        copy("")
        opened.grid.gridView.paste(nil)
        XCTAssertFalse(model.hasUnsavedEdits)
        XCTAssertFalse(opened.undo.canUndo)

        // Delete over the short row: its own cell empties, the hatched ones
        // stay missing; undone, the row reads as before.
        select(opened, (1, 0), (1, 2))
        opened.grid.gridView.delete(nil)
        XCTAssertEqual(row(model, 1), ["", "", ""])
        XCTAssertTrue(model.isHatched(row: 1, column: 1), "still hatched")
        XCTAssertEqual(opened.undo.undoActionName, "Clear Cells")
        opened.undo.undo()
        XCTAssertEqual(row(model, 1), ["4", "", ""])
        XCTAssertFalse(model.hasUnsavedEdits)
    }

    /// Paste is refused, with an alert saying why, for a block past the
    /// last row or column, and past an unterminated quote; nothing changes
    /// and no undo step is left.
    func testABlockThatDoesNotFitIsRefusedSayingWhy() async throws {
        let opened = try await open(file("fit.csv", csv))
        let model = opened.model
        opened.grid.select(CellPosition(row: 2, column: 0))
        copy("a\nb\n")
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(alerts, ["The copied cells run past the last row. Paste them higher up, or insert rows first."])
        opened.grid.select(CellPosition(row: 0, column: 2))
        copy("a\tb")
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(alerts.last, "The copied cells run past the last column. Paste them further left, or insert columns first.")
        XCTAssertFalse(model.hasUnsavedEdits)
        XCTAssertFalse(opened.undo.canUndo)

        let quoted = try await open(file("quote.csv", "a,b,c\n1,2,3\n4,\"open\n5,6\n"))
        select(quoted, (0, 2), (1, 2))
        copy("x\ny")
        quoted.grid.gridView.paste(nil)
        XCTAssertEqual(alerts.last, "A quote earlier in a row is never closed, so the cells after it are inside the quote and can’t be changed.")
        XCTAssertFalse(quoted.model.hasUnsavedEdits)
        XCTAssertEqual(value(quoted.model, 0, 2), "3")
    }

    // MARK: Clear

    /// ⌫ and ⌦ in the grid clear the selection, one step each; a held key
    /// clears once. In the in-cell editor ⌫ deletes text, not cells; in
    /// the find bar Delete and Paste are the field's.
    func testTheDeleteKeysClearTheGridButNotTheEditors() async throws {
        let opened = try await open(file("keys.csv", csv))
        let (model, content, window) = (opened.model, opened.content, opened.window)
        window.makeFirstResponder(opened.grid.gridView)
        select(opened, (0, 1), (1, 1))
        opened.grid.gridView.keyDown(with: try deleteKey(window))
        XCTAssertEqual(value(model, 0, 1), "")
        XCTAssertEqual(value(model, 1, 1), "")
        XCTAssertEqual(opened.undo.undoActionName, "Clear Cells")
        let steps = opened.document.history.journal.count
        opened.grid.gridView.keyDown(with: try deleteKey(window, repeating: true))
        XCTAssertEqual(opened.document.history.journal.count, steps, "a repeat does nothing")
        opened.grid.select(CellPosition(row: 2, column: 2))
        opened.grid.gridView.keyDown(with: try deleteKey(window, forward: true))
        XCTAssertEqual(value(model, 2, 2), "")
        opened.undo.undo()
        opened.undo.undo()
        XCTAssertFalse(model.hasUnsavedEdits)

        // The in-cell editor: ⌫ is the text's.
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.editActiveCell()
        await content.cellEditor.loading?.value
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        XCTAssertTrue(window.firstResponder === editor)
        editor.setSelectedRange(NSRange(location: 6, length: 0))
        editor.keyDown(with: try deleteKey(window))
        XCTAssertEqual(content.cellEditor.field.stringValue, "Marlo")
        XCTAssertEqual(value(model, 1, 1), "Ostrava", "no cell cleared")
        XCTAssertFalse(model.hasUnsavedEdits)
        content.cellEditor.cancel()

        // The find bar: the grid isn't the first responder, so Paste and
        // Delete go to the field.
        content.showFind(nil)
        XCTAssertTrue(window.firstResponder is NSText)
        XCTAssertFalse(window.firstResponder === opened.grid.gridView)
    }

    /// Clearing more cells than the core clears at once (⌘A on a large
    /// file) is off, with the reason as Delete's tooltip; the key says it
    /// in an alert, and nothing changes.
    func testClearingTooManyCellsIsRefused() async throws {
        var text = "a,b,c,d,e,f,g,h,i,j\n"
        for row in 0..<10_001 { text += (0..<10).map { "\(row).\($0)" }.joined(separator: ",") + "\n" }
        let opened = try await open(file("many.csv", text))
        let (model, window) = (opened.model, opened.window)
        opened.grid.gridView.selectAll(nil)
        let limit = "Leal clears up to 100,000 cells at a time. To empty a whole column, delete it and insert an empty one."
        let (enabled, tooltip) = validate(opened, #selector(GridView.delete(_:)))
        XCTAssertFalse(enabled)
        XCTAssertEqual(tooltip, limit)
        window.makeFirstResponder(opened.grid.gridView)
        opened.grid.gridView.keyDown(with: try deleteKey(window))
        XCTAssertEqual(alerts, [limit])
        XCTAssertFalse(model.hasUnsavedEdits)
        // A paste of one value into all of them likewise.
        copy("x")
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(alerts.last, "Leal pastes up to 100,000 cells at a time.")
        XCTAssertFalse(model.hasUnsavedEdits)
    }

    // MARK: When they are off

    /// While a save runs, Paste and Delete are off, saying why; the keys
    /// beep, and VoiceOver hears why. While the file is being read too
    /// (the core's `StillReading`, its tests).
    func testOffWhileSaving() async throws {
        let opened = try await open(file("save.csv", csv))
        let (model, content) = (opened.model, opened.content)
        _ = model.setCell(.cell(CellPosition(row: 0, column: 1)), to: "Marlowe")
        opened.grid.select(CellPosition(row: 1, column: 1))
        copy("x")
        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        for action in [#selector(GridView.paste(_:)), #selector(GridView.delete(_:))] {
            let (enabled, tooltip) = validate(opened, action)
            XCTAssertFalse(enabled)
            XCTAssertEqual(tooltip, "Wait for the save to finish.")
        }
        opened.grid.gridView.paste(nil)
        opened.grid.gridView.delete(nil)
        XCTAssertEqual(content.lastAnnouncement, "Wait for the save to finish.")
        XCTAssertTrue(alerts.isEmpty)
        XCTAssertEqual(value(model, 1, 1), "Ostrava")
        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)
        XCTAssertTrue(validate(opened, #selector(GridView.paste(_:))).0)
        XCTAssertTrue(validate(opened, #selector(GridView.delete(_:))).0)
        // With nothing on the clipboard, Paste is off.
        pasteboard.clearContents()
        XCTAssertFalse(validate(opened, #selector(GridView.paste(_:))).0)
    }

    // MARK: Find, and the bytes

    /// Find catches up with a paste and a clear, and their undo, without
    /// starting again.
    func testFindCatchesUpAfterPasteAndClear() async throws {
        let opened = try await open(file("find.csv", "id,name,city\n1,Marlow,Leeds\n2,Ostrava,York\n3,Halden,York\n"))
        let (content, find) = (opened.content, opened.content.find)
        content.showFind(nil)
        content.findBar.field.stringValue = "marlow"
        content.search(for: "marlow")
        try await waitUntil("searched") { !find.isSearching && find.pendingStep == nil }
        XCTAssertEqual(find.matchCount, 1)
        let search = try XCTUnwrap(find.search)
        select(opened, (1, 1), (2, 2))
        copy("Marlow\tmarlow\nx\tMARLOW")
        opened.grid.gridView.paste(nil)
        try await waitUntil("caught up") { find.progress?.catchingUp == false && find.matchCount == 4 }
        XCTAssertTrue(find.search === search, "not restarted")
        XCTAssertNotNil(find.highlight(row: 2, column: 2))
        select(opened, (0, 1), (1, 1))
        opened.grid.gridView.delete(nil)
        try await waitUntil("caught up with the clear") { find.progress?.catchingUp == false && find.matchCount == 2 }
        opened.undo.undo()
        opened.undo.undo()
        try await waitUntil("caught up with the undo") { find.progress?.catchingUp == false && find.matchCount == 1 }
        XCTAssertTrue(find.search === search)
    }

    /// Only the cells pasted or cleared change on disk: quoted fields,
    /// escaped quotes and CRLFs elsewhere stay as they were; values that
    /// need quotes are quoted; a pasted multi-line value keeps its line
    /// break.
    func testOnlyThePastedAndClearedCellsChangeOnDisk() async throws {
        let text = "id,name,note\r\n1,\"Marlow\",\"say \"\"hi\"\"\"\r\n2, spaced ,\"multi\r\nline\"\r\n3,plain,last\r\n"
        let opened = try await open(file("bytes.csv", text))
        select(opened, (0, 1), (0, 1))
        // As Numbers or Excel copy two cells, one of them multi-line.
        copy("a, comma\t\"two\nlines\"\r\n")
        opened.grid.gridView.paste(nil)
        select(opened, (2, 2), (2, 2))
        opened.grid.gridView.delete(nil)
        opened.grid.select(CellPosition(row: 1, column: 1))
        copy("says \"hi\"")
        opened.grid.gridView.paste(nil)
        let saved = try await save(opened)
        XCTAssertEqual(
            String(decoding: saved, as: UTF8.self),
            "id,name,note\r\n1,\"a, comma\",\"two\nlines\"\r\n2,\"says \"\"hi\"\"\",\"multi\r\nline\"\r\n3,plain,\r\n"
        )
    }

    // MARK: Cost

    /// What Paste and Clear of 100,000 cells (the most at once) cost the
    /// main thread, the model's catching up included: printed for the
    /// task notes, and well under a second.
    func testOneHundredThousandCellsTakeWellUnderASecond() async throws {
        var text = "a,b,c,d,e,f,g,h,i,j\n"
        for row in 0..<10_000 { text += (0..<10).map { "value \(row).\($0)" }.joined(separator: ",") + "\n" }
        let opened = try await open(file("cost.csv", text))
        let model = opened.model
        opened.grid.gridView.selectAll(nil)
        var clock = ContinuousClock.now
        opened.grid.gridView.delete(nil)
        let clear = ContinuousClock.now - clock
        XCTAssertEqual(value(model, 9_999, 9), "")
        clock = ContinuousClock.now
        opened.undo.undo()
        let undoClear = ContinuousClock.now - clock
        XCTAssertEqual(value(model, 9_999, 9), "value 9999.9")

        copy((0..<10_000).map { row in (0..<10).map { "pasted \(row).\($0)" }.joined(separator: "\t") }.joined(separator: "\n"))
        opened.grid.select(CellPosition(row: 0, column: 0))
        clock = ContinuousClock.now
        opened.grid.gridView.paste(nil)
        let paste = ContinuousClock.now - clock
        XCTAssertEqual(value(model, 9_999, 9), "pasted 9999.9")
        print("2.6 cost: clear 100k cells \(clear), its undo \(undoClear), paste 100k cells \(paste)")
        XCTAssertTrue(alerts.isEmpty, "\(alerts)")
        XCTAssertLessThan(clear, .seconds(1))
        XCTAssertLessThan(paste, .seconds(1))
    }
}
