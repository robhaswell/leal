import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.5a against the real core, hosted in Leal.app: the Edit menu's
/// Insert Row Above and Below (⌘↩), Delete Row (⌘⌫), Insert Column Before
/// and After and Delete Column; undo, redo and the journal; the header row;
/// the selection afterwards; Find catching up after rows and restarting
/// after a column (ADR-0014 decision 2); the commands off, with the reason,
/// while the file is read or saved and after an unterminated quote; ⌘⌫ and
/// ⌘↩ in the editors and the find bar; and the bytes saved afterwards,
/// where only the rows and columns touched change.
///
/// No sheet is shown (`CSVDocument.showSheet` is replaced) and nothing is
/// sent to the system: key events are made here and handed to the views
/// (CLAUDE.md).
@MainActor
final class StructureTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-structure-\(UUID().uuidString)")
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
    }

    override func tearDown() async throws {
        debugReleaseHeldSave()
        DocumentModel.openForTesting = nil
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
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

    private func open(_ url: URL, waitForIndex: Bool = true) async throws -> Opened {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        addTeardownBlock { @MainActor in withExtendedLifetime(document) {} }
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        if waitForIndex {
            try await waitUntil("indexed") { model.isIndexComplete }
        }
        document.isOnScreen = { _ in true }
        document.showSheet = { alert, _, done in
            XCTFail("unexpected sheet: \(alert.messageText)")
            done(NSApplication.ModalResponse(rawValue: NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + alert.buttons.count - 1))
        }
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

    private func value(_ model: DocumentModel, _ row: Int, _ column: Int) -> String? {
        model.fullValue(.cell(CellPosition(row: row, column: column)))
    }

    /// Grid column `column`, top to bottom.
    private func column(_ model: DocumentModel, _ column: Int) -> [String?] {
        (0..<model.rowCount).map { value(model, $0, column) }
    }

    private func item(_ command: StructureCommand) -> NSMenuItem {
        NSMenuItem(title: "", action: DocumentViewController.action(command), keyEquivalent: "")
    }

    /// Whether the window's Edit menu item for `command` is on, and its
    /// tooltip.
    private func validate(_ opened: Opened, _ command: StructureCommand) -> (Bool, String?) {
        let item = item(command)
        let enabled = opened.content.validateMenuItem(item)
        return (enabled, item.toolTip)
    }

    /// Saves as ⌘S does, and waits for it.
    private func save(_ opened: Opened) async throws -> Data {
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        let saved = await saving.value
        XCTAssertTrue(saved, "saved")
        return try Data(contentsOf: try XCTUnwrap(opened.document.fileURL))
    }

    private func key(_ characters: String, code: UInt16, window: NSWindow) throws -> NSEvent {
        try XCTUnwrap(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: .command, timestamp: 0, windowNumber: window.windowNumber,
            context: nil, characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code
        ))
    }

    // MARK: The menu

    /// The Edit menu has the six commands, ⌘↩ and ⌘⌫ on Insert Row Below
    /// and Delete Row (DESIGN §4.2), with no target (they reach the front
    /// window's view controller), and no other item takes those keys.
    func testTheEditMenuHasTheRowAndColumnCommands() throws {
        let bar = MainMenu.make()
        let edit = try XCTUnwrap(bar.items.compactMap(\.submenu).first { $0.title == "Edit" })
        let expected: [(String, StructureCommand, String)] = [
            ("Insert Row Above", .insertRowAbove, ""),
            ("Insert Row Below", .insertRowBelow, "\r"),
            ("Delete Row", .deleteRows, "\u{8}"),
            ("Insert Column Before", .insertColumnBefore, ""),
            ("Insert Column After", .insertColumnAfter, ""),
            ("Delete Column", .deleteColumns, ""),
        ]
        for (title, command, key) in expected {
            let item = try XCTUnwrap(edit.items.first { $0.action == DocumentViewController.action(command) }, title)
            XCTAssertEqual(item.title, title)
            XCTAssertEqual(item.keyEquivalent, key, title)
            XCTAssertEqual(item.keyEquivalentModifierMask, .command, title)
            XCTAssertNil(item.target, title)
        }
        let keys = bar.items.compactMap(\.submenu).flatMap(\.items)
        for key in ["\r", "\u{8}"] {
            XCTAssertEqual(keys.filter { $0.keyEquivalent == key }.count, 1, "one item takes ⌘\(key.debugDescription)")
        }
    }

    // MARK: Rows

    func testInsertRowBelowAndAboveSelectTheNewRowAndUndo() async throws {
        let opened = try await open(file("rows.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        opened.grid.select(CellPosition(row: 0, column: 1))

        XCTAssertEqual(validate(opened, .insertRowBelow).0, true)
        content.insertRowBelow(nil)
        XCTAssertEqual(model.rowCount, 4)
        XCTAssertEqual(column(model, 1), ["Marlow", "", "Ostrava", "Halden"])
        XCTAssertEqual(value(model, 1, 2), "", "as many cells as the other rows")
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 1), "the new row, to type into")
        XCTAssertTrue(model.hasUnsavedEdits)
        XCTAssertEqual(undo.undoActionName, "Insert Row")
        XCTAssertEqual(opened.document.history.journal.map(\.direction), [.edit])
        XCTAssertNotNil(opened.document.history.journal.first?.command.structural)

        undo.undo()
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Halden"])
        XCTAssertFalse(model.hasUnsavedEdits)
        undo.redo()
        XCTAssertEqual(column(model, 1), ["Marlow", "", "Ostrava", "Halden"])
        XCTAssertEqual(opened.document.history.journal.map(\.direction), [.edit, .undo, .redo])
        undo.undo()

        // Above the first data row: after the header row, which stays.
        opened.grid.select(CellPosition(row: 0, column: 2))
        content.insertRowAbove(nil)
        XCTAssertEqual(model.headerTitle(column: 1).text, "name")
        XCTAssertEqual(column(model, 1), ["", "Marlow", "Ostrava", "Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 2))

        // Below the last row: at the end.
        opened.grid.select(CellPosition(row: 3, column: 0))
        content.insertRowBelow(nil)
        XCTAssertEqual(column(model, 1), ["", "Marlow", "Ostrava", "Halden", ""])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 4, column: 0))
    }

    func testDeleteRowDeletesTheSelectedRows() async throws {
        let opened = try await open(file("rows.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        opened.grid.select(CellPosition(row: 0, column: 1))
        opened.grid.extend(to: CellPosition(row: 1, column: 2))
        let item = item(.deleteRows)
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.title, "Delete Rows", "named for the rows selected")

        content.deleteRows(nil)
        XCTAssertEqual(column(model, 1), ["Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 1), "the row that took their place")
        XCTAssertEqual(undo.undoActionName, "Delete Rows")
        undo.undo()
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Halden"])
        XCTAssertFalse(model.hasUnsavedEdits, "their own bytes are back")
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 1))

        // The last row: the row before it is selected.
        opened.grid.select(CellPosition(row: 2, column: 0))
        _ = content.validateMenuItem(item)
        XCTAssertEqual(item.title, "Delete Row")
        content.deleteRows(nil)
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 0))

        // Every row: nothing to select, and the header row stays.
        opened.grid.selectAll()
        content.deleteRows(nil)
        XCTAssertEqual(model.rowCount, 0)
        XCTAssertNil(opened.grid.activeCell)
        XCTAssertEqual(model.headerTitle(column: 0).text, "id")
        XCTAssertFalse(validate(opened, .deleteRows).0, "nothing to delete")
        XCTAssertTrue(validate(opened, .insertRowBelow).0, "a row can be added")
        content.insertRowBelow(nil)
        XCTAssertEqual(model.rowCount, 1)
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 0))
    }

    // MARK: Columns

    func testInsertAndDeleteColumns() async throws {
        let opened = try await open(file("columns.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        opened.grid.select(CellPosition(row: 1, column: 1))

        content.insertColumnAfter(nil)
        XCTAssertEqual(model.columnCount, 4)
        XCTAssertEqual((0..<4).map { model.headerTitle(column: $0).text }, ["id", "name", "", "qty"])
        XCTAssertEqual(column(model, 2), ["", "", ""])
        XCTAssertEqual(column(model, 3), ["3", "5", "8"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 2), "the new column")
        XCTAssertEqual(model.columnWidths.count, 4)
        XCTAssertEqual(undo.undoActionName, "Insert Column")
        undo.undo()
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(column(model, 2), ["3", "5", "8"])

        opened.grid.select(CellPosition(row: 0, column: 0))
        content.insertColumnBefore(nil)
        XCTAssertEqual(column(model, 0), ["", "", ""])
        XCTAssertEqual(column(model, 1), ["1", "2", "3"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 0))
        undo.undo()

        // Two columns selected: both go, as one step.
        opened.grid.select(CellPosition(row: 2, column: 0))
        opened.grid.extend(to: CellPosition(row: 2, column: 1))
        let item = item(.deleteColumns)
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.title, "Delete Columns")
        content.deleteColumns(nil)
        XCTAssertEqual(model.columnCount, 1)
        XCTAssertEqual(model.headerTitle(column: 0).text, "qty")
        XCTAssertEqual(column(model, 0), ["3", "5", "8"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 2, column: 0))
        XCTAssertEqual(undo.undoActionName, "Delete Columns")
        XCTAssertEqual(opened.document.history.journal.count, 6, "two inserts, their undos, and a command per column")
        undo.undo()
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual((0..<3).map { model.headerTitle(column: $0).text }, ["id", "name", "qty"])
        XCTAssertFalse(model.hasUnsavedEdits, "one step put both back")
        undo.redo()
        XCTAssertEqual(model.columnCount, 1)
        XCTAssertEqual(column(model, 0), ["3", "5", "8"])
    }

    // MARK: Find (ADR-0014 decision 2)

    func testFindCatchesUpAfterRowsAndRestartsAfterAColumn() async throws {
        let text = "id,name,city\n1,Marlow,Leeds\n2,Ostrava,Marlow\n3,Halden,York\n"
        let opened = try await open(file("find.csv", text))
        let (model, content, find) = (opened.model, opened.content, opened.content.find)
        content.showFind(nil)
        content.findBar.field.stringValue = "marlow"
        content.search(for: "marlow")
        try await waitUntil("searched") { !find.isSearching && find.pendingStep == nil }
        XCTAssertEqual(find.matchCount, 2)
        let search = try XCTUnwrap(find.search)

        // Deleting a row with a match: the same search, one match fewer.
        opened.grid.select(CellPosition(row: 1, column: 0))
        content.deleteRows(nil)
        try await waitUntil("caught up") { find.progress?.catchingUp == false && find.matchCount == 1 }
        XCTAssertTrue(find.search === search, "not restarted")
        XCTAssertEqual(find.highlight(row: 0, column: 1)?.ranges, [NSRange(location: 0, length: 6)])
        XCTAssertNil(find.highlight(row: 1, column: 1), "the row after moved up, with its highlights")

        // An inserted row is searched too, once typed into.
        content.insertRowBelow(nil)
        XCTAssertTrue(find.search === search)
        _ = model.setCell(.cell(try XCTUnwrap(opened.grid.activeCell)), to: "Marlow again")
        try await waitUntil("caught up") { find.progress?.catchingUp == false && find.matchCount == 2 }
        XCTAssertTrue(find.search === search)

        // A column: searched again from the start, the selection kept.
        opened.grid.select(CellPosition(row: 0, column: 2))
        content.insertColumnBefore(nil)
        XCTAssertFalse(find.search === search, "restarted")
        try await waitUntil("searched again") { !find.isSearching }
        XCTAssertEqual(find.matchCount, 2)
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 2))
        XCTAssertNotNil(find.highlight(row: 0, column: 1))
        let restarted = find.search
        opened.undo.undo()
        XCTAssertFalse(find.search === restarted, "an undo of a column restarts it too")
        _ = model
    }

    // MARK: When they are off

    /// While the file is still being read (here, from a simulated network
    /// share whose copy is held part-way: ADR-0014 decision 1 waits for
    /// it), every command is off, saying why, and ⌘↩ and ⌘⌫ only beep.
    /// Opened as Finder opens files (off the main thread, as a share must
    /// be).
    func testOffUntilTheWholeFileIsRead() async throws {
        var text = "id,name\n"
        for row in 0..<30000 { text += "\(row),name \(row)\n" }
        let url = try file("share.csv", text)
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentSimulatingShare(
                path: path, temp: environment.temp, scheduler: environment.scheduler, options: options, observer: observer,
                chunkBytes: 32_768, readDelayMs: 0, failure: nil, holdAt: 100_000
            )
        }
        let opened: NSDocument = try await withCheckedThrowingContinuation { continuation in
            NSDocumentController.shared.openDocument(withContentsOf: url, display: false) { document, _, error in
                if let document {
                    continuation.resume(returning: document)
                } else {
                    continuation.resume(throwing: error ?? NSError(domain: "StructureTests", code: 1))
                }
            }
        }
        let document = try XCTUnwrap(opened as? CSVDocument)
        if document.windowControllers.isEmpty { document.makeWindowControllers() }
        let content = try XCTUnwrap((document.windowControllers.first as? DocumentWindowController)?.content)
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        content.grid.select(CellPosition(row: 0, column: 0))
        XCTAssertFalse(model.isIndexComplete)
        let reason = "Rows and columns can be inserted and deleted once Leal has read the whole file."
        for command in StructureCommand.allCases {
            let item = item(command)
            XCTAssertFalse(content.validateMenuItem(item), "\(command)")
            XCTAssertEqual(item.toolTip, reason, "\(command)")
        }
        let rows = model.rowCount
        content.rowCommandKey(insert: true)
        content.rowCommandKey(insert: false)
        content.insertColumnAfter(nil)
        XCTAssertEqual(content.lastAnnouncement, reason)
        XCTAssertEqual(model.rowCount, rows)
        XCTAssertFalse(model.hasUnsavedEdits)

        model.backgroundHandle()?.debugShareRelease()
        try await waitUntil("read") { model.isIndexComplete && model.rowDeleteRefusal() == nil }
        for command in StructureCommand.allCases {
            let item = item(command)
            XCTAssertTrue(content.validateMenuItem(item), "\(command)")
            XCTAssertNil(item.toolTip, "\(command)")
        }
    }

    /// While a save runs, they are off, saying why (cell edits go on).
    func testOffWhileSaving() async throws {
        let opened = try await open(file("save.csv", csv))
        let (model, content) = (opened.model, opened.content)
        _ = model.setCell(.cell(CellPosition(row: 0, column: 1)), to: "Marlowe")
        opened.grid.select(CellPosition(row: 1, column: 1))
        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        for command in StructureCommand.allCases {
            let (enabled, tooltip) = validate(opened, command)
            XCTAssertFalse(enabled, "\(command)")
            XCTAssertEqual(tooltip, "Wait for the save to finish.", "\(command)")
        }
        content.insertRowBelow(nil)
        XCTAssertEqual(model.rowCount, 3, "refused")
        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)
        XCTAssertTrue(validate(opened, .insertRowBelow).0)
    }

    /// After an unterminated quote (ADR-0004 decision 8), a row or a
    /// column can't go after it, and the menu item says why; before it,
    /// they can.
    func testInsertingAfterAnUnterminatedQuoteIsOff() async throws {
        let opened = try await open(file("open.csv", "a,b\n1,2\n3,\"never closed\n4,5\n"))
        let model = opened.model
        XCTAssertEqual(model.rowCount, 2)
        opened.grid.select(CellPosition(row: 1, column: 1))
        let (below, belowReason) = validate(opened, .insertRowBelow)
        XCTAssertFalse(below)
        XCTAssertEqual(belowReason, "A quote in the last row is never closed, so a row inserted after it would be part of its text.")
        let (after, afterReason) = validate(opened, .insertColumnAfter)
        XCTAssertFalse(after)
        XCTAssertEqual(afterReason, "A quote in the last row is never closed, so a column inserted after it would be part of its text.")
        XCTAssertTrue(validate(opened, .insertRowAbove).0)
        XCTAssertTrue(validate(opened, .insertColumnBefore).0)
        XCTAssertTrue(validate(opened, .deleteRows).0)

        opened.content.rowCommandKey(insert: true)
        XCTAssertEqual(model.rowCount, 2, "⌘↩ beeps")
        XCTAssertEqual(opened.content.lastAnnouncement, belowReason)
        opened.content.insertRowAbove(nil)
        XCTAssertEqual(model.rowCount, 3)
        XCTAssertEqual(value(model, 2, 1), "never closed\n4,5\n")
    }

    // MARK: Keys

    /// ⌘⌫ in the in-cell editor deletes to the start of the line, as in any
    /// text field, never a row; ⌘↩ there commits the edit and inserts a row
    /// below. In the find bar both are the field's: the commands are off.
    /// In the grid, ⌘ with the keypad's Enter (which isn't the menu item's
    /// key) inserts a row too.
    func testRowKeysInTheEditorsAndTheFindBar() async throws {
        let opened = try await open(file("keys.csv", csv))
        let (model, content, window) = (opened.model, opened.content, opened.window)
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.editActiveCell()
        await content.cellEditor.loading?.value
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        XCTAssertTrue(editor is LiteralTextView)
        XCTAssertTrue(window.firstResponder === editor)
        editor.selectAll(nil)
        editor.insertText("Marlow Bros", replacementRange: editor.selectedRange())
        XCTAssertTrue(validate(opened, .deleteRows).0, "a click on the menu item still works")

        XCTAssertTrue(editor.performKeyEquivalent(with: try key("\u{7f}", code: 51, window: window)))
        XCTAssertEqual(content.cellEditor.field.stringValue, "", "deleted to the start of the line")
        XCTAssertEqual(model.rowCount, 3, "no row deleted")
        XCTAssertFalse(editor.performKeyEquivalent(with: try key("\r", code: 36, window: window)), "⌘↩ goes on to the menu")

        // The menu's Insert Row Below commits the edit first.
        editor.insertText("Marlowe", replacementRange: editor.selectedRange())
        content.insertRowBelow(nil)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(column(model, 1), ["Marlowe", "", "Ostrava", "Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 1))

        // ⌘ and the keypad's Enter in the grid.
        window.makeFirstResponder(opened.grid.gridView)
        opened.grid.gridView.keyDown(with: try key("\u{3}", code: 76, window: window))
        XCTAssertEqual(model.rowCount, 5)
        XCTAssertFalse(content.cellEditor.isEditing, "not Return's edit")
        opened.grid.gridView.keyDown(with: try key("\u{7f}", code: 51, window: window))
        XCTAssertEqual(model.rowCount, 4)

        content.showFind(nil)
        XCTAssertTrue(window.firstResponder is NSText)
        for command in StructureCommand.allCases {
            XCTAssertFalse(validate(opened, command).0, "\(command) leaves its keys to the find bar")
        }
    }

    // MARK: The bytes saved

    /// After each command and a save, only the rows or the column it
    /// touched differ on disk: every other byte, quotes, CRLFs and all, is
    /// the file's own (DESIGN §5).
    func testSavingChangesOnlyTheRowsAndColumnsTouched() async throws {
        let cases: [(String, (Opened) -> Void, String)] = [
            ("insert row", { opened in
                opened.grid.select(CellPosition(row: 0, column: 1))
                opened.content.insertRowBelow(nil)
                _ = opened.model.setCell(.cell(CellPosition(row: 1, column: 0)), to: "9")
            }, "id,name,qty\r\n1,Marlow,3\r\n9,,\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n"),
            ("delete row", { opened in
                opened.grid.select(CellPosition(row: 1, column: 1))
                opened.content.deleteRows(nil)
            }, "id,name,qty\r\n1,Marlow,3\r\n3,Halden,8\r\n"),
            ("insert column", { opened in
                opened.grid.select(CellPosition(row: 0, column: 2))
                opened.content.insertColumnAfter(nil)
            }, "id,name,qty,\r\n1,Marlow,3,\r\n2,\"Ostrava\",5,\r\n3,Halden,8,\r\n"),
            ("delete column", { opened in
                opened.grid.select(CellPosition(row: 0, column: 1))
                opened.content.deleteColumns(nil)
            }, "id,qty\r\n1,3\r\n2,5\r\n3,8\r\n"),
        ]
        for (index, (name, act, expected)) in cases.enumerated() {
            let opened = try await open(file("bytes-\(index).csv", csv))
            act(opened)
            XCTAssertTrue(opened.model.hasUnsavedEdits, name)
            let saved = try await save(opened)
            XCTAssertEqual(String(decoding: saved, as: UTF8.self), expected, name)
            opened.document.close()
        }
    }

    /// Odd bytes in rows left alone stay: an escaped quote, spaces, a lone
    /// CRLF among LFs. An inserted row ends with the file's usual line
    /// ending; a deleted row takes its own with it.
    func testSavingKeepsOddBytesInRowsLeftAlone() async throws {
        let text = "a,b\n1,\"x\"\"y\"\n2,  spaced \r\n3,z\n"
        let opened = try await open(file("odd.csv", text))
        opened.grid.select(CellPosition(row: 1, column: 0))
        opened.content.deleteRows(nil)
        opened.grid.select(CellPosition(row: 0, column: 0))
        opened.content.insertRowBelow(nil)
        let saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), "a,b\n1,\"x\"\"y\"\n,\n3,z\n")
    }
}
