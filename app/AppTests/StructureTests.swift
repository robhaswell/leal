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

    private func key(
        _ characters: String, code: UInt16, window: NSWindow, shift: Bool = false, repeating: Bool = false, also: NSEvent.ModifierFlags = []
    ) throws -> NSEvent {
        let flags: NSEvent.ModifierFlags = shift ? [.command, .shift] : .command
        return try XCTUnwrap(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags.union(also), timestamp: 0, windowNumber: window.windowNumber,
            context: nil, characters: characters, charactersIgnoringModifiers: characters, isARepeat: repeating, keyCode: code
        ))
    }

    // MARK: The menu

    /// The Edit menu has the seven commands, in order, ⌘↩, ⇧⌘↩ and ⌘⌫ on
    /// Insert Row Below, Duplicate Row and Delete Row (DESIGN §4.2), with
    /// no target (they reach the front window's view controller), and no
    /// other item takes those keys.
    func testTheEditMenuHasTheRowAndColumnCommands() throws {
        let bar = MainMenu.make()
        let edit = try XCTUnwrap(bar.items.compactMap(\.submenu).first { $0.title == "Edit" })
        let shift: NSEvent.ModifierFlags = [.command, .shift]
        let expected: [(String, StructureCommand, String, NSEvent.ModifierFlags)] = [
            ("Insert Row Above", .insertRowAbove, "", .command),
            ("Insert Row Below", .insertRowBelow, "\r", .command),
            ("Duplicate Row", .duplicateRows, "\r", shift),
            ("Delete Row", .deleteRows, "\u{8}", .command),
            ("Insert Column Before", .insertColumnBefore, "", .command),
            ("Insert Column After", .insertColumnAfter, "", .command),
            ("Delete Column", .deleteColumns, "", .command),
        ]
        var indexes: [Int] = []
        for (title, command, key, modifiers) in expected {
            let index = try XCTUnwrap(edit.items.firstIndex { $0.action == DocumentViewController.action(command) }, title)
            let item = edit.items[index]
            indexes.append(index)
            XCTAssertEqual(item.title, title)
            XCTAssertEqual(item.keyEquivalent, key, title)
            XCTAssertEqual(item.keyEquivalentModifierMask, modifiers, title)
            XCTAssertNil(item.target, title)
        }
        XCTAssertEqual(indexes, indexes.sorted(), "in order")
        XCTAssertEqual(indexes[3] - indexes[0], 3, "the row items together")
        let keys = bar.items.compactMap(\.submenu).flatMap(\.items)
        for (key, modifiers) in [("\r", NSEvent.ModifierFlags.command), ("\r", shift), ("\u{8}", .command)] {
            let taking = keys.filter { $0.keyEquivalent == key && $0.keyEquivalentModifierMask == modifiers }
            XCTAssertEqual(taking.count, 1, "one item takes \(modifiers) \(key.debugDescription)")
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
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 0), "the new row's first cell, to type into")
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
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 0))

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
        // A file with only a header row has no cell to insert a column
        // beside: off, and it says what to do.
        for command in [StructureCommand.insertColumnBefore, .insertColumnAfter] {
            let (enabled, tooltip) = validate(opened, command)
            XCTAssertFalse(enabled, "\(command)")
            XCTAssertEqual(tooltip, "A column is inserted beside the selected cell, and a file with only a header row has no cell. Insert a row first.", "\(command)")
        }
        XCTAssertFalse(validate(opened, .deleteColumns).0)
        content.insertRowBelow(nil)
        XCTAssertEqual(model.rowCount, 1)
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 0, column: 0))
    }

    // MARK: Duplicate Row

    /// Duplicate Row (⇧⌘↩) puts a copy of the row below it and selects it,
    /// in the same column; undo and redo keep the step's name. Several
    /// rows: copies of each after the last, as one step, selected in the
    /// same columns.
    func testDuplicateRowCopiesTheRowsAndKeepsTheColumn() async throws {
        let opened = try await open(file("duplicate.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        _ = model.setCell(.cell(CellPosition(row: 1, column: 2)), to: "6")
        opened.grid.select(CellPosition(row: 1, column: 2))
        let item = item(.duplicateRows)
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.title, "Duplicate Row")

        content.duplicateRows(nil)
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Ostrava", "Halden"])
        XCTAssertEqual(column(model, 2), ["3", "6", "6", "8"], "the row as it reads, edits and all")
        XCTAssertEqual(opened.grid.selection, GridSelection(CellPosition(row: 2, column: 2)), "the copy, same column")
        XCTAssertEqual(undo.undoActionName, "Duplicate Row")
        XCTAssertEqual(opened.document.history.journal.map(\.direction), [.edit, .edit])
        undo.undo()
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Halden"])
        XCTAssertEqual(undo.redoActionName, "Duplicate Row")
        undo.redo()
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Ostrava", "Halden"])
        XCTAssertEqual(undo.undoActionName, "Duplicate Row")
        undo.undo()

        // Two rows, two columns: copies after the second, selected alike.
        opened.grid.select(CellPosition(row: 1, column: 2))
        opened.grid.extend(to: CellPosition(row: 0, column: 1))
        _ = content.validateMenuItem(item)
        XCTAssertEqual(item.title, "Duplicate Rows")
        content.duplicateRows(nil)
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Marlow", "Ostrava", "Halden"])
        XCTAssertEqual(
            opened.grid.selection,
            GridSelection(active: CellPosition(row: 3, column: 2), anchor: CellPosition(row: 3, column: 2), extent: CellPosition(row: 2, column: 1))
        )
        XCTAssertEqual(opened.grid.selection?.rows, 2...3)
        XCTAssertEqual(undo.undoActionName, "Duplicate Rows")
        undo.undo()
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Halden"])
        XCTAssertEqual(undo.redoActionName, "Duplicate Rows")

        // ⇧⌘↩ in the grid (its item off, or the keypad's Enter), held too.
        opened.window.makeFirstResponder(opened.grid.gridView)
        opened.grid.select(CellPosition(row: 2, column: 1))
        opened.grid.gridView.keyDown(with: try key("\r", code: 36, window: opened.window, shift: true))
        opened.grid.gridView.keyDown(with: try key("\u{3}", code: 76, window: opened.window, shift: true, repeating: true))
        XCTAssertEqual(column(model, 1), ["Marlow", "Ostrava", "Halden", "Halden", "Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 4, column: 1))
        XCTAssertFalse(content.cellEditor.isEditing, "not Return's edit")
        XCTAssertEqual(undo.undoActionName, "Duplicate Row")
    }

    /// The copy's fields are its row's, byte for byte: quotes, an escaped
    /// quote, spaces, an edited cell quoted as its field was; its line
    /// ending is the file's most common (here every row's CRLF). Undone
    /// and redone after a save (by value, ADR-0014 decisions 3 and 8), the
    /// file goes back to its bytes, then to the copy's.
    func testADuplicateSavesItsRowsBytes() async throws {
        let text = "id,name,note\r\n1,\"Ostrava\",\"say \"\"hi\"\"\"\r\n2,  spaced ,x\r\n"
        let opened = try await open(file("dup-bytes.csv", text))
        _ = opened.model.setCell(.cell(CellPosition(row: 0, column: 1)), to: "Brno")
        let edited = "id,name,note\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n2,  spaced ,x\r\n"
        let doubled = "id,name,note\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n2,  spaced ,x\r\n"
        opened.grid.select(CellPosition(row: 0, column: 1))
        opened.content.duplicateRows(nil)
        var saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), doubled)
        opened.undo.undo()
        saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), edited, "undone")
        opened.undo.redo()
        XCTAssertEqual(opened.undo.undoActionName, "Duplicate Row")
        saved = try await save(opened)
        XCTAssertEqual(String(decoding: saved, as: UTF8.self), doubled, "redone")

        // Two rows, one a spaced one: both copied after the second.
        opened.grid.select(CellPosition(row: 1, column: 0))
        opened.grid.extend(to: CellPosition(row: 2, column: 0))
        opened.content.duplicateRows(nil)
        saved = try await save(opened)
        XCTAssertEqual(
            String(decoding: saved, as: UTF8.self),
            "id,name,note\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n2,  spaced ,x\r\n1,\"Brno\",\"say \"\"hi\"\"\"\r\n2,  spaced ,x\r\n"
        )
    }

    /// Duplicate Row copies at most 10,000 rows at once (the core reads
    /// them on the main thread): with more selected (⌘A, or a whole column
    /// to the last row) it is off, saying so, and its key beeps with the
    /// reason; 10,000 can be.
    func testDuplicateIsOffPastTenThousandRows() async throws {
        var text = "id,name\n"
        for row in 0..<10_001 { text += "\(row),n\(row)\n" }
        let opened = try await open(file("many.csv", text))
        let (model, content, grid) = (opened.model, opened.content, opened.grid)
        XCTAssertEqual(model.rowCount, 10_001)
        let reason = "Duplicate up to 10,000 rows at a time."
        grid.select(CellPosition(row: 0, column: 1))
        grid.selectAll()
        XCTAssertEqual(validate(opened, .duplicateRows).0, false)
        XCTAssertEqual(validate(opened, .duplicateRows).1, reason)
        XCTAssertTrue(validate(opened, .deleteRows).0, "only Duplicate Row has a limit")
        opened.window.makeFirstResponder(grid.gridView)
        grid.gridView.keyDown(with: try key("\r", code: 36, window: opened.window, shift: true))
        grid.gridView.keyDown(with: try key("\u{3}", code: 76, window: opened.window, shift: true))
        XCTAssertEqual(model.rowCount, 10_001, "⇧⌘↩ beeps")
        XCTAssertEqual(content.lastAnnouncement, reason)
        content.duplicateRows(nil)
        XCTAssertEqual(model.rowCount, 10_001, "refused by the core too")
        XCTAssertFalse(opened.undo.canUndo)

        // A whole column, to the last row (⇧⌘↓).
        grid.select(CellPosition(row: 0, column: 0))
        grid.extend(to: CellPosition(row: 10_000, column: 0), throughLastRow: true)
        XCTAssertEqual(validate(opened, .duplicateRows).1, reason)

        // 10,000 rows can be.
        grid.select(CellPosition(row: 1, column: 0))
        grid.extend(to: CellPosition(row: 10_000, column: 0))
        XCTAssertEqual(validate(opened, .duplicateRows).0, true)
        content.duplicateRows(nil)
        XCTAssertEqual(model.rowCount, 20_001)
        XCTAssertEqual(value(model, 10_001, 1), "n1")
        XCTAssertEqual(opened.undo.undoActionName, "Duplicate Rows")
    }

    /// A duplicate refused on the keypad's path (⇧⌘ and its Enter, in the
    /// grid where the item is off, and in the in-cell editor, where it runs
    /// the command and the core refuses it) leaves no step: Redo, and its
    /// name, are still there.
    func testARefusedDuplicateByTheKeypadKeepsRedo() async throws {
        let opened = try await open(file("open-redo.csv", "a,b\n1,2\n3,\"never closed\n4,5\n"))
        let (model, content, window, undo) = (opened.model, opened.content, opened.window, opened.undo)
        _ = model.setCell(.cell(CellPosition(row: 0, column: 0)), to: "9")
        undo.undo()
        XCTAssertEqual(undo.redoActionName, "Typing")
        opened.grid.select(CellPosition(row: 1, column: 0))
        window.makeFirstResponder(opened.grid.gridView)
        opened.grid.gridView.keyDown(with: try key("\u{3}", code: 76, window: window, shift: true))
        XCTAssertEqual(model.rowCount, 2)
        XCTAssertTrue(undo.canRedo)
        XCTAssertEqual(undo.redoActionName, "Typing")

        content.editActiveCell()
        await content.cellEditor.loading?.value
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        XCTAssertTrue(editor.performKeyEquivalent(with: try key("\u{3}", code: 76, window: window, shift: true)))
        XCTAssertEqual(model.rowCount, 2, "the core refused it")
        XCTAssertTrue(undo.canRedo)
        XCTAssertEqual(undo.redoActionName, "Typing")
        XCTAssertFalse(undo.canUndo)
        undo.redo()
        XCTAssertEqual(value(model, 0, 0), "9")
    }

    /// In the inspector, ⇧⌘ and the keypad's Enter commits the edit, then
    /// duplicates the row, as ⇧⌘↩ does there: it puts in no line break.
    func testShiftCommandKeypadEnterInTheInspectorDuplicates() async throws {
        let opened = try await open(file("inspector.csv", csv))
        let (model, content, window) = (opened.model, opened.content, opened.window)
        content.setInspectorShown(true)
        opened.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        let text = content.inspector.textView
        window.makeFirstResponder(text)
        text.insertText("Marlowe", replacementRange: NSRange(location: 0, length: (text.string as NSString).length))
        XCTAssertTrue(text.performKeyEquivalent(with: try key("\u{3}", code: 76, window: window, shift: true, also: .numericPad)))
        XCTAssertEqual(column(model, 1), ["Marlowe", "Marlowe", "Ostrava", "Halden"], "committed, then copied; no line break")
        XCTAssertEqual(opened.undo.undoActionName, "Duplicate Row")
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 1))
    }

    /// Caps Lock doesn't change the keys: ⌘↩, ⇧⌘↩ and ⌘⌫ are still the
    /// row commands', ⇧↩ still a line break, the inspector's ⌘↩ still
    /// commits.
    func testCapsLockLeavesTheKeysAlone() throws {
        let window = NSWindow(contentRect: .zero, styleMask: [], backing: .buffered, defer: true)
        XCTAssertEqual(GridView.rowCommandKey(try key("\r", code: 36, window: window, also: .capsLock)), .insertBelow)
        XCTAssertEqual(GridView.rowCommandKey(try key("\u{3}", code: 76, window: window, also: [.capsLock, .numericPad])), .insertBelow)
        XCTAssertEqual(GridView.rowCommandKey(try key("\r", code: 36, window: window, shift: true, also: .capsLock)), .duplicate)
        XCTAssertEqual(GridView.rowCommandKey(try key("\u{7f}", code: 51, window: window, also: .capsLock)), .delete)
        XCTAssertNil(GridView.rowCommandKey(try key("\r", code: 36, window: window, also: [.capsLock, .option])))
        XCTAssertTrue(InspectorTextView.isCommit(try key("\r", code: 36, window: window, also: .capsLock)))
        XCTAssertFalse(InspectorTextView.isCommit(try key("\r", code: 36, window: window, shift: true, also: .capsLock)))
        let shiftReturn = try XCTUnwrap(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: [.shift, .capsLock], timestamp: 0, windowNumber: 0,
            context: nil, characters: "\r", charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36
        ))
        XCTAssertTrue(LiteralTextView.isShiftReturn(shiftReturn))
        XCTAssertFalse(LiteralTextView.isShiftReturn(try key("\r", code: 36, window: window, shift: true, also: .capsLock)))
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
        XCTAssertEqual(undo.redoActionName, "Delete Columns", "the group's name, not its last command's")
        undo.redo()
        XCTAssertEqual(undo.undoActionName, "Delete Columns")
        XCTAssertEqual(model.columnCount, 1)
        XCTAssertEqual(column(model, 0), ["3", "5", "8"])
    }

    /// Several columns are one step in the document's history even with
    /// the view in no window (its own undo manager is the window's), and a
    /// refusal, foreseen or not, leaves no step: no nameless Undo, and
    /// Redo as it was when foreseen.
    func testDeleteColumnsIsOneStepOrNone() async throws {
        let opened = try await open(file("step.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        _ = model.setCell(.cell(CellPosition(row: 0, column: 0)), to: "10")
        _ = model.setCell(.cell(CellPosition(row: 1, column: 0)), to: "20")
        undo.undo()
        let (undoName, redoName) = (undo.undoActionName, undo.redoActionName)
        XCTAssertTrue(undo.canRedo)

        // Refused (a save runs): asked first, so no group was opened.
        opened.grid.select(CellPosition(row: 0, column: 0))
        opened.grid.extend(to: CellPosition(row: 0, column: 1))
        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        content.deleteColumns(nil)
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(content.lastAnnouncement, "Wait for the save to finish.")
        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)
        XCTAssertEqual(undo.undoActionName, undoName)
        XCTAssertEqual(undo.redoActionName, redoName)
        XCTAssertTrue(undo.canRedo, "Redo kept")

        // Refused by the command itself, past the question: the empty
        // group goes again.
        opened.grid.select(CellPosition(row: 0, column: 0))
        opened.grid.extend(to: CellPosition(row: 0, column: 1))
        model.refusalForTesting = .saving
        content.deleteColumns(nil)
        model.refusalForTesting = nil
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(undo.undoActionName, undoName, "no nameless step")
        XCTAssertTrue(undo.canRedo, "Redo kept")
        XCTAssertEqual(undo.redoActionName, redoName)

        // No window: still both, as one step of the document's history.
        opened.window.contentView = NSView()
        XCTAssertNil(content.view.window)
        content.deleteColumns(nil)
        XCTAssertEqual(model.columnCount, 1)
        XCTAssertEqual(undo.undoActionName, "Delete Columns")
        undo.undo()
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(undo.redoActionName, "Delete Columns")
    }

    /// Rows deleted (or put back) at logical row 0 before the header row
    /// was turned on: undone and redone with it on, the header titles are
    /// read again (they were the old row's), and the change is the first
    /// data row's (not row −1).
    func testUndoingRowsAtTheHeaderRowReadsItsTitlesAgain() async throws {
        let opened = try await open(file("header.csv", "1,2\n3,4\n5,6\n"))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        XCTAssertFalse(model.interpretation.header)
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.deleteRows(nil)
        XCTAssertEqual(column(model, 0), ["3", "5"])
        content.toggleHeaderRow(nil)
        try await waitUntil("read with a header row") {
            model.interpretation.header && model.isIndexComplete && model.headerTitle(column: 0).text == "3"
        }
        XCTAssertEqual(column(model, 0), ["5"])
        var changes: [StructureChange] = []
        let heard = model.onChange
        model.onChange = { change in
            if case let .structure(structure) = change { changes.append(structure) }
            heard?(change)
        }

        undo.undo()
        XCTAssertEqual((0..<2).map { model.headerTitle(column: $0).text }, ["1", "2"])
        XCTAssertEqual(column(model, 0), ["3", "5"])
        XCTAssertEqual(changes.last, StructureChange(column: nil, row: 0, inserted: true))
        XCTAssertEqual(opened.grid.activeCell?.row, 0)
        undo.redo()
        XCTAssertEqual((0..<2).map { model.headerTitle(column: $0).text }, ["3", "4"])
        XCTAssertEqual(column(model, 0), ["5"])
        XCTAssertEqual(changes.last, StructureChange(column: nil, row: 0, inserted: false))
    }

    // MARK: Undo and redo after a save

    /// A row delete and a two-column delete, each saved, undone and saved,
    /// redone and saved: the file goes back to its own bytes, then to the
    /// delete's again (undo by value after a save, ADR-0014 decision 3).
    func testUndoAndRedoAfterASaveWriteTheBytesBack() async throws {
        let cases: [(String, (Opened) -> Void, String)] = [
            ("delete row", { opened in
                opened.grid.select(CellPosition(row: 1, column: 1))
                opened.content.deleteRows(nil)
            }, "id,name,qty\r\n1,Marlow,3\r\n3,Halden,8\r\n"),
            ("delete columns", { opened in
                opened.grid.select(CellPosition(row: 0, column: 0))
                opened.grid.extend(to: CellPosition(row: 0, column: 1))
                opened.content.deleteColumns(nil)
            }, "qty\r\n3\r\n5\r\n8\r\n"),
        ]
        for (index, (name, act, deleted)) in cases.enumerated() {
            let opened = try await open(file("again-\(index).csv", csv))
            act(opened)
            var saved = try await save(opened)
            XCTAssertEqual(String(decoding: saved, as: UTF8.self), deleted, name)
            opened.undo.undo()
            XCTAssertTrue(opened.model.hasUnsavedEdits, name)
            saved = try await save(opened)
            XCTAssertEqual(String(decoding: saved, as: UTF8.self), csv, "\(name): undone")
            opened.undo.redo()
            saved = try await save(opened)
            XCTAssertEqual(String(decoding: saved, as: UTF8.self), deleted, "\(name): redone")
            opened.document.close()
        }
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

        // Its undo and redo: caught up with too, the same search.
        opened.undo.undo()
        XCTAssertNil(find.pendingStep)
        try await waitUntil("caught up with the undo") { find.progress?.catchingUp == false && find.matchCount == 2 }
        XCTAssertTrue(find.search === search, "not restarted by the undo")
        XCTAssertEqual(find.highlight(row: 1, column: 2)?.ranges, [NSRange(location: 0, length: 6)])
        opened.undo.redo()
        try await waitUntil("caught up with the redo") { find.progress?.catchingUp == false && find.matchCount == 1 }
        XCTAssertTrue(find.search === search, "not restarted by the redo")

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
        content.rowCommandKey(.insertRowBelow)
        content.rowCommandKey(.deleteRows)
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
        XCTAssertEqual(
            belowReason,
            "A quote in the last row is never closed, so a row inserted after it would land inside the quote. Insert Row Above still works, and the cell can still be edited."
        )
        let (after, afterReason) = validate(opened, .insertColumnAfter)
        XCTAssertFalse(after)
        XCTAssertEqual(
            afterReason,
            "A quote in the last row is never closed, so a column inserted after it would land inside the quote. Insert Column Before still works, and the cell can still be edited."
        )
        let (duplicate, duplicateReason) = validate(opened, .duplicateRows)
        XCTAssertFalse(duplicate, "a copy would follow the quote's row")
        XCTAssertEqual(
            duplicateReason,
            "A quote in the last row is never closed, so a copy of the row would land inside the quote. Insert Row Above still works, and the cell can still be edited."
        )
        XCTAssertTrue(validate(opened, .insertRowAbove).0)
        XCTAssertTrue(validate(opened, .insertColumnBefore).0)
        XCTAssertTrue(validate(opened, .deleteRows).0)

        opened.content.rowCommandKey(.insertRowBelow)
        XCTAssertEqual(model.rowCount, 2, "⌘↩ beeps")
        XCTAssertEqual(opened.content.lastAnnouncement, belowReason)
        opened.content.rowCommandKey(.duplicateRows)
        XCTAssertEqual(model.rowCount, 2, "⇧⌘↩ beeps")
        XCTAssertEqual(opened.content.lastAnnouncement, duplicateReason)
        // Rows before the quote's row can be duplicated; with it, not.
        opened.grid.select(CellPosition(row: 0, column: 0))
        XCTAssertTrue(validate(opened, .duplicateRows).0)
        opened.grid.extend(to: CellPosition(row: 1, column: 0))
        XCTAssertFalse(validate(opened, .duplicateRows).0)
        opened.grid.select(CellPosition(row: 1, column: 1))
        opened.content.insertRowAbove(nil)
        XCTAssertEqual(model.rowCount, 3)
        XCTAssertEqual(value(model, 2, 1), "never closed\n4,5\n")
    }

    // MARK: Keys

    /// ⌘⌫ in the in-cell editor deletes to the start of the line, as in any
    /// text field, never a row; ⌘↩ (⇧⌘↩) there commits the edit and
    /// inserts a row below (duplicates the row). In the find bar all are
    /// the field's: the commands are off. In the grid, ⌘ with the keypad's
    /// Enter (which isn't the menu item's key) inserts a row too.
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
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 0))

        // ⌘ and the keypad's Enter in the in-cell editor: as ⌘↩, the edit
        // committed, then a row inserted below.
        opened.grid.select(CellPosition(row: 1, column: 1))
        content.editActiveCell()
        await content.cellEditor.loading?.value
        let again = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        again.insertText("Bree", replacementRange: NSRange(location: 0, length: (again.string as NSString).length))
        XCTAssertTrue(again.performKeyEquivalent(with: try key("\u{3}", code: 76, window: window)))
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(column(model, 1), ["Marlowe", "Bree", "", "Ostrava", "Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 2, column: 0))

        // ⌘ and the keypad's Enter in the grid.
        window.makeFirstResponder(opened.grid.gridView)
        opened.grid.gridView.keyDown(with: try key("\u{3}", code: 76, window: window))
        XCTAssertEqual(model.rowCount, 6)
        XCTAssertFalse(content.cellEditor.isEditing, "not Return's edit")
        opened.grid.gridView.keyDown(with: try key("\u{7f}", code: 51, window: window))
        XCTAssertEqual(model.rowCount, 5)
        // A held ⌘⌫'s repeats delete nothing more.
        opened.grid.gridView.keyDown(with: try key("\u{7f}", code: 51, window: window, repeating: true))
        XCTAssertEqual(model.rowCount, 5)

        // ⇧⌘↩ in the in-cell editor goes on to the menu (which commits the
        // edit, then duplicates the row); ⇧⌘ and the keypad's Enter does
        // both there.
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.editActiveCell()
        await content.cellEditor.loading?.value
        let third = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        third.insertText("Marlow", replacementRange: NSRange(location: 0, length: (third.string as NSString).length))
        XCTAssertFalse(third.performKeyEquivalent(with: try key("\r", code: 36, window: window, shift: true)), "⇧⌘↩ goes on to the menu")
        XCTAssertTrue(third.performKeyEquivalent(with: try key("\u{3}", code: 76, window: window, shift: true)))
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(column(model, 1), ["Marlow", "Marlow", "Bree", "", "Ostrava", "Halden"])
        XCTAssertEqual(opened.grid.activeCell, CellPosition(row: 1, column: 1), "the copy, same column")
        XCTAssertEqual(opened.undo.undoActionName, "Duplicate Row")

        // A sheet over the window (Go to Row's) with its field focused:
        // the field keeps the keys.
        let sheet = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 200, height: 80), styleMask: [.titled], backing: .buffered, defer: true)
        let field = NSTextField(frame: NSRect(x: 10, y: 10, width: 120, height: 24))
        sheet.contentView?.addSubview(field)
        sheet.makeFirstResponder(field)
        XCTAssertTrue(sheet.firstResponder is NSText)
        content.keyWindow = { sheet }
        for command in StructureCommand.allCases {
            XCTAssertFalse(validate(opened, command).0, "\(command) leaves its keys to the sheet's field")
        }
        content.keyWindow = { nil }
        XCTAssertTrue(validate(opened, .deleteRows).0)

        content.showFind(nil)
        XCTAssertTrue(window.firstResponder is NSText)
        for command in StructureCommand.allCases {
            let (enabled, tooltip) = validate(opened, command)
            XCTAssertFalse(enabled, "\(command) leaves its keys to the find bar")
            XCTAssertEqual(tooltip, "Click the table first.", "\(command)")
        }
        window.makeFirstResponder(opened.grid.gridView)
        XCTAssertTrue(validate(opened, .deleteRows).0)
        XCTAssertNil(validate(opened, .deleteRows).1, "the reason goes with the focus")
    }

    /// With the inspector focused but with nothing it can edit (a missing
    /// cell's note), ⌘⌫ is Delete Row's, as its menu item (on) says; while
    /// the inspector edits, it is the text's.
    func testCommandDeleteInTheInspectorNotEditingDeletesTheRow() async throws {
        let opened = try await open(file("inspector-keys.csv", "a,b,c\n1,2,3\n4\n5,6,7\n"))
        let (model, content, window) = (opened.model, opened.content, opened.window)
        try await waitUntil("the short row known") { model.isHatched(row: 1, column: 1) }
        content.setInspectorShown(true)
        // Editing: the text's.
        opened.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        let text = content.inspector.textView
        window.makeFirstResponder(text)
        XCTAssertTrue(text.isEditable)
        XCTAssertTrue(text.performKeyEquivalent(with: try key("\u{7f}", code: 51, window: window)))
        XCTAssertEqual(model.rowCount, 3, "no row deleted")

        // A missing cell: a note, nothing to edit.
        window.makeFirstResponder(opened.grid.gridView)
        opened.grid.select(CellPosition(row: 1, column: 1))
        await content.inspectorTask?.value
        window.makeFirstResponder(text)
        XCTAssertTrue(window.firstResponder === text)
        XCTAssertFalse(text.isEditable, "showing a note")
        XCTAssertTrue(validate(opened, .deleteRows).0, "the menu item is on")
        XCTAssertTrue(text.performKeyEquivalent(with: try key("\u{7f}", code: 51, window: window)))
        XCTAssertEqual(model.rowCount, 2, "the row went, as the menu item says")
        XCTAssertEqual(opened.undo.undoActionName, "Delete Row")
        XCTAssertEqual(content.lastAnnouncement, "Row deleted")
    }

    /// VoiceOver hears what a successful insert, duplicate or delete did.
    func testVoiceOverHearsWhatChanged() async throws {
        let opened = try await open(file("announce.csv", csv))
        let content = opened.content
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.insertRowAbove(nil)
        XCTAssertEqual(content.lastAnnouncement, "Row inserted")
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.insertRowBelow(nil)
        XCTAssertEqual(content.lastAnnouncement, "Row inserted")
        opened.grid.select(CellPosition(row: 2, column: 1))
        content.duplicateRows(nil)
        XCTAssertEqual(content.lastAnnouncement, "Row duplicated")
        opened.grid.select(CellPosition(row: 0, column: 1))
        opened.grid.extend(to: CellPosition(row: 2, column: 1))
        content.duplicateRows(nil)
        XCTAssertEqual(content.lastAnnouncement, "3 rows duplicated")
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.deleteRows(nil)
        XCTAssertEqual(content.lastAnnouncement, "Row deleted")
        opened.grid.select(CellPosition(row: 0, column: 1))
        opened.grid.extend(to: CellPosition(row: 3, column: 1))
        content.deleteRows(nil)
        XCTAssertEqual(content.lastAnnouncement, "4 rows deleted")

        opened.grid.select(CellPosition(row: 0, column: 1))
        content.insertColumnBefore(nil)
        XCTAssertEqual(content.lastAnnouncement, "Column inserted")
        content.insertColumnAfter(nil)
        XCTAssertEqual(content.lastAnnouncement, "Column inserted")
        opened.grid.select(CellPosition(row: 0, column: 1))
        content.deleteColumns(nil)
        XCTAssertEqual(content.lastAnnouncement, "Column deleted")
        opened.grid.select(CellPosition(row: 0, column: 0))
        opened.grid.extend(to: CellPosition(row: 0, column: 1))
        content.deleteColumns(nil)
        XCTAssertEqual(content.lastAnnouncement, "2 columns deleted")
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
