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

        // "" into the hatched cells (a spreadsheet's copy of an empty
        // cell): no edit, no undo step.
        select(opened, (1, 1), (1, 2))
        copy("\n")
        opened.grid.gridView.paste(nil)
        XCTAssertFalse(model.hasUnsavedEdits)
        XCTAssertFalse(opened.undo.canUndo)

        // Empty text pastes nothing (Rob, 2026-10-04): the cells aren't
        // cleared.
        select(opened, (0, 0), (0, 2))
        copy("")
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(row(model, 0), ["1", "2", "3"])
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
    /// need quotes are quoted; a pasted multi-line value's LF becomes the
    /// file's CRLF, as a typed line break would (Rob, 2026-10-04).
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
            "id,name,note\r\n1,\"a, comma\",\"two\r\nlines\"\r\n2,\"says \"\"hi\"\"\",\"multi\r\nline\"\r\n3,plain,\r\n"
        )
    }

    /// A line break inside a pasted value is the file's own line ending:
    /// CRLF in a CRLF file, LF in an LF file, whichever the clipboard had.
    func testLineBreaksInPastedValuesAreTheFiles() async throws {
        for (name, text, line) in [("crlf.csv", "a,b\r\n1,2\r\n", "\r\n"), ("lf.csv", "a,b\n1,2\n", "\n")] {
            let opened = try await open(file(name, text))
            opened.grid.select(CellPosition(row: 0, column: 0))
            copy("\"lf\nx\"\t\"cr\rx\"\r\n")
            opened.grid.gridView.paste(nil)
            copy("\"crlf\r\nx\"")
            opened.grid.select(CellPosition(row: 0, column: 1))
            opened.grid.gridView.paste(nil)
            let saved = try await save(opened)
            XCTAssertEqual(String(decoding: saved, as: UTF8.self), "a,b\(line)\"lf\(line)x\",\"crlf\(line)x\"\(line)", name)
        }
    }

    /// Of the clipboard's types, the tab-separated one is read first.
    func testTheTabSeparatedTypeIsPreferredOverPlainText() async throws {
        let opened = try await open(file("types.csv", csv))
        opened.grid.select(CellPosition(row: 0, column: 1))
        pasteboard.clearContents()
        pasteboard.declareTypes([.string, .tabularText], owner: nil)
        pasteboard.setString("plain text", forType: .string)
        pasteboard.setString("A\tB", forType: .tabularText)
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(row(opened.model, 0), ["1", "A", "B"])
    }

    /// More text than the core takes is refused before it is handed over,
    /// with an alert.
    func testTooMuchTextIsRefusedBeforeTheCore() async throws {
        let opened = try await open(file("much.csv", csv))
        opened.grid.select(CellPosition(row: 0, column: 1))
        copy(String(repeating: "x", count: Int(pasteByteLimit()) + 1))
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(alerts, ["Leal pastes up to 33.6 MB of text at a time."])
        XCTAssertFalse(opened.model.hasUnsavedEdits)
    }

    /// Undo of a paste after a Save, into a short row's hatched cells too:
    /// the cells read as before. The saved file has the hatched cells as
    /// fields now, and no cell edit shortens a row, so undo empties them
    /// (as for typing, task 2.2: `holds` in the core); saving again writes
    /// every other byte as it was.
    func testUndoOfAPasteAfterASave() async throws {
        let text = "a,b,c\n1,2,3\n4\n5,6,7\n"
        let opened = try await open(file("after-save.csv", text))
        let model = opened.model
        try await waitUntil("the short row known") { model.isHatched(row: 1, column: 1) }
        select(opened, (0, 1), (0, 1))
        copy("x\ty\nz\tw")
        opened.grid.gridView.paste(nil)
        var saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), "a,b,c\n1,x,y\n4,z,w\n5,6,7\n")
        XCTAssertEqual(opened.undo.undoActionName, "Paste")
        opened.undo.undo()
        XCTAssertEqual(row(model, 0), ["1", "2", "3"])
        XCTAssertEqual(row(model, 1), ["4", "", ""])
        XCTAssertFalse(model.isHatched(row: 1, column: 1), "a field of the saved file, emptied")
        saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), "a,b,c\n1,2,3\n4,,\n5,6,7\n")
        opened.undo.redo()
        XCTAssertEqual(row(model, 1), ["4", "z", "w"])
    }

    /// Edit > Delete reaches the grid through the menu's own item (the
    /// standard `delete:`), and clears.
    func testEditDeleteClearsThroughTheMenu() async throws {
        let opened = try await open(file("menu.csv", csv))
        let window = opened.window
        window.makeFirstResponder(opened.grid.gridView)
        select(opened, (0, 1), (1, 1))
        let edit = try XCTUnwrap(NSApp.mainMenu?.items.first { $0.submenu?.title == "Edit" }?.submenu)
        for title in ["Cut", "Paste", "Delete"] {
            let item = try XCTUnwrap(edit.items.first { $0.title == title }, title)
            XCTAssertTrue(handler(of: try XCTUnwrap(item.action), in: window) === opened.grid.gridView, "\(title) reaches the grid")
        }
        let delete = try XCTUnwrap(edit.items.first { $0.title == "Delete" })
        XCTAssertTrue(opened.grid.gridView.validateMenuItem(delete))
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(delete.action), to: opened.grid.gridView, from: delete))
        XCTAssertEqual([value(opened.model, 0, 1), value(opened.model, 1, 1)], ["", ""])
        XCTAssertEqual(opened.undo.undoActionName, "Clear Cells")
    }

    /// The responder that takes `action`, as AppKit looks for it from the
    /// window's first responder.
    private func handler(of action: Selector, in window: NSWindow) -> NSResponder? {
        var responder = window.firstResponder
        while let current = responder, !current.responds(to: action) {
            responder = current.nextResponder
        }
        return responder
    }

    /// With the inspector's text or Go to Row's field focused, ⌫, ⌘X, ⌘V
    /// and Edit > Delete are the text's: they never reach the grid.
    func testTheInspectorAndGoToRowKeepTheirKeys() async throws {
        let opened = try await open(file("focus.csv", csv))
        let (model, content, window) = (opened.model, opened.content, opened.window)
        content.setInspectorShown(true)
        opened.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        let text = content.inspector.textView
        window.makeFirstResponder(text)
        for action in [#selector(NSText.cut(_:)), #selector(NSText.paste(_:)), #selector(NSText.delete(_:)), #selector(NSResponder.deleteBackward(_:))] {
            XCTAssertTrue(handler(of: action, in: window) === text, "\(action) is the inspector's")
        }
        text.setSelectedRange(NSRange(location: 6, length: 0))
        text.keyDown(with: try deleteKey(window))
        XCTAssertEqual(text.string, "Marlo")
        XCTAssertEqual(value(model, 0, 1), "Marlow", "no cell cleared")
        XCTAssertFalse(model.hasUnsavedEdits)

        let (alert, field) = content.goToRowAlert()
        alert.layout()
        let sheet = alert.window
        sheet.makeFirstResponder(field)
        let editor = try XCTUnwrap(sheet.firstResponder as? NSText)
        for action in [#selector(NSText.cut(_:)), #selector(NSText.paste(_:)), #selector(NSText.delete(_:)), #selector(NSResponder.deleteBackward(_:))] {
            XCTAssertTrue(handler(of: action, in: sheet) === editor, "\(action) is Go to Row's field's")
        }
    }

    // MARK: Cut

    /// Cut of cells: the cells go on the clipboard as Copy puts them, then
    /// are emptied, as one undo step named Cut; the bytes saved change only
    /// there; undo puts them back.
    func testCutOfCellsCopiesThenClearsAsOneStep() async throws {
        let opened = try await open(file("cut.csv", csv))
        let (model, content) = (opened.model, opened.content)
        select(opened, (0, 1), (1, 2))
        content.copySelection()
        let copied = pasteboard.string(forType: .string)
        pasteboard.clearContents()
        XCTAssertTrue(validate(opened, #selector(GridView.cut(_:))).0)
        let journal = opened.document.history.journal.count
        opened.grid.gridView.cut(nil)
        XCTAssertEqual(pasteboard.string(forType: .string), copied)
        XCTAssertEqual(pasteboard.string(forType: .tabularText), copied)
        XCTAssertEqual(copied, "Marlow\t3\nOstrava\t5")
        XCTAssertEqual(row(model, 0), ["1", "", ""])
        XCTAssertEqual(row(model, 1), ["2", "", ""])
        XCTAssertEqual(opened.document.history.journal.count, journal + 1, "one command")
        XCTAssertEqual(opened.undo.undoActionName, "Cut")
        let saved = try await save(opened)
        // A quoted field stays quoted, as for typing.
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), "id,name,qty\r\n1,,\r\n2,\"\",\r\n3,Halden,8\r\n")
        opened.undo.undo()
        XCTAssertEqual(row(model, 1), ["2", "Ostrava", "5"])
        XCTAssertEqual(opened.undo.redoActionName, "Cut")
        // Pasted back where it was.
        opened.undo.redo()
        select(opened, (0, 1), (0, 1))
        opened.grid.gridView.paste(nil)
        XCTAssertEqual(row(model, 1), ["2", "Ostrava", "5"])
        XCTAssertTrue(alerts.isEmpty, "\(alerts)")
    }

    /// Cut of whole rows (picked by their row numbers): they go on the
    /// clipboard, every column, and are deleted, as one step named Cut.
    func testCutOfWholeRowsDeletesThem() async throws {
        let opened = try await open(file("cut-rows.csv", csv))
        let model = opened.model
        opened.grid.selectRow(0)
        opened.grid.extendRows(to: 1)
        XCTAssertEqual(opened.grid.selection?.wholeRows, true)
        opened.grid.gridView.cut(nil)
        XCTAssertEqual(pasteboard.string(forType: .string), "1\tMarlow\t3\n2\tOstrava\t5")
        XCTAssertEqual(model.rowCount, 1)
        XCTAssertEqual(row(model, 0), ["3", "Halden", "8"])
        XCTAssertEqual(opened.undo.undoActionName, "Cut")
        let saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), "id,name,qty\r\n3,Halden,8\r\n")
        opened.undo.undo()
        XCTAssertEqual(model.rowCount, 3)
        XCTAssertEqual(row(model, 1), ["2", "Ostrava", "5"])
        let again = try await save(opened)
        XCTAssertEqual(String(decoding: again, as: UTF8.self), csv)

        // ⇧↓ keeps whole rows whole; ⇧→ makes them cells.
        let rows = GridSelection.row(0, columns: 3, column: 1)
        XCTAssertTrue(rows.extended(to: CellPosition(row: 1, column: 2)).wholeRows)
        XCTAssertFalse(rows.extended(to: CellPosition(row: 0, column: 1)).wholeRows)
        XCTAssertFalse(GridSelection.all(rows: 3, columns: 3, active: CellPosition(row: 0, column: 0)).wholeRows, "⌘A is cells")
    }

    /// A Cut that can't delete or clear copies nothing: too many cells (an
    /// alert, and the tooltip), or a save running (off, saying why).
    func testARefusedCutCopiesNothing() async throws {
        var text = "a,b,c,d,e,f,g,h,i,j\n"
        for row in 0..<10_001 { text += (0..<10).map { "\(row).\($0)" }.joined(separator: ",") + "\n" }
        let opened = try await open(file("cut-many.csv", text))
        let model = opened.model
        copy("kept")
        opened.grid.gridView.selectAll(nil)
        let limit = "Leal cuts up to 100,000 cells at a time. To cut whole rows, select them by their row numbers."
        XCTAssertEqual(validate(opened, #selector(GridView.cut(_:))).1, limit)
        opened.grid.gridView.cut(nil)
        XCTAssertEqual(alerts, [limit])
        XCTAssertEqual(pasteboard.string(forType: .string), "kept")
        XCTAssertFalse(model.hasUnsavedEdits)

        opened.grid.select(CellPosition(row: 1, column: 1))
        _ = model.setCell(.cell(CellPosition(row: 0, column: 1)), to: "edited")
        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        XCTAssertEqual(validate(opened, #selector(GridView.cut(_:))).1, "Wait for the save to finish.")
        opened.grid.gridView.cut(nil)
        XCTAssertEqual(opened.content.lastAnnouncement, "Wait for the save to finish.")
        XCTAssertEqual(pasteboard.string(forType: .string), "kept")
        XCTAssertEqual(value(model, 1, 1), "1.1")
        debugReleaseHeldSave()
        _ = await saving.value
    }

    /// Cut of cells holding more text than Leal replaces at once is
    /// refused when the clear finds it, and copies nothing.
    func testACutOfTooMuchTextCopiesNothing() async throws {
        let big = String(repeating: "v", count: Int(pasteByteLimit()) / 3 + 1)
        let opened = try await open(file("cut-big.csv", "a,b\n" + String(repeating: "\(big),x\n", count: 3)))
        copy("kept")
        // Every row, whether or not "a,b" is taken for a header row.
        select(opened, (0, 0), (3, 0))
        opened.grid.gridView.cut(nil)
        XCTAssertEqual(alerts, [
            "The selected cells hold more than 33.6 MB of text, more than Leal clears at a time. To empty a whole column, delete it and insert an empty one.",
        ])
        XCTAssertEqual(pasteboard.string(forType: .string), "kept")
        XCTAssertFalse(opened.model.hasUnsavedEdits)
        XCTAssertFalse(opened.undo.canUndo)
    }

    // MARK: Recover changes

    /// A paste and a clear of more than 256 cells (kept in Rust) come back
    /// through Recover changes as steps named Paste and Clear Cells, and
    /// the recovery report names a batch by its first cell.
    func testLargeBatchesAfterRecover() async throws {
        var text = "a,b\n"
        for row in 0..<400 { text += "\(row),x\n" }
        let opened = try await open(file("recover.csv", text))
        let (document, model) = (opened.document, opened.model)
        document.showSheet = { _, _, _ in }
        opened.grid.select(CellPosition(row: 0, column: 0))
        copy((0..<300).map { "p\($0)\tq\($0)" }.joined(separator: "\n"))
        opened.grid.gridView.paste(nil)
        select(opened, (300, 0), (399, 1))
        opened.grid.gridView.delete(nil)
        XCTAssertEqual(value(model, 299, 1), "q299")
        XCTAssertEqual(value(model, 350, 0), "")
        let batch = try XCTUnwrap(document.history.journal.first?.applied)
        XCTAssertNotNil(batch.cells, "kept in Rust")
        XCTAssertEqual(HistoryText.describe(batch, header: true), "Row 1, column 1 and 599 more cells")

        _ = model.call { try $0.debugPanic() }
        await document.recoverChanges()
        let report = try XCTUnwrap(document.lastRecovery)
        XCTAssertTrue(report.refused.isEmpty, "\(report.refused)")
        XCTAssertEqual(value(model, 299, 1), "q299")
        XCTAssertEqual(value(model, 350, 0), "")
        XCTAssertEqual(opened.undo.undoActionName, "Clear Cells")
        opened.undo.undo()
        XCTAssertEqual(value(model, 350, 0), "350")
        XCTAssertEqual(opened.undo.undoActionName, "Paste")
        opened.undo.undo()
        XCTAssertEqual(value(model, 299, 1), "x")
        XCTAssertFalse(model.hasUnsavedEdits)
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
