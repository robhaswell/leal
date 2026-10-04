import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.5.2 against the real core, hosted in Leal.app: undo and redo of
/// cell, batch and structural commands through the document's undo
/// manager; the change count and "— Edited"; the edited-cell marks; Treat
/// As and Reopen with Encoding off while there are edits; undo cleared by a
/// new split; Reload and Close with edits; and Recover changes after a
/// failure (`debugPanic`), with the file unchanged and changed.
///
/// No sheet is shown: `CSVDocument.showSheet` and the close prompt are
/// replaced, and nothing is sent to the system (CLAUDE.md).
@MainActor
final class UndoTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-undo-\(UUID().uuidString)")
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
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private let csv = "id,name,qty\n1,Marlow,3\n2,Ostrava,5\n3,Halden,8\n"

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
    }

    private func open(_ url: URL) async throws -> Opened {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
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

    /// Grid cell (`row`, `column`)'s value as the core holds it.
    private func value(_ model: DocumentModel, _ row: Int, _ column: Int) -> String? {
        model.fullValue(.cell(CellPosition(row: row, column: column)))
    }

    @discardableResult
    private func set(_ model: DocumentModel, _ row: Int, _ column: Int, _ text: String) throws -> EditCommand {
        guard case let .edited(command) = model.setCell(.cell(CellPosition(row: row, column: column)), to: text) else {
            throw XCTSkip("the edit wasn't made")
        }
        return command
    }

    /// A command made in the core directly (a batch, rows, a column: the
    /// app makes these from tasks 2.5a and 2.6), applied as the app will.
    private func apply(_ model: DocumentModel, _ make: (LealFFI.Document) throws -> EditCommand?) throws {
        let command = try XCTUnwrap(model.call { try make($0) } ?? nil)
        model.commandApplied(command, as: .edit)
    }

    private func menuItem(_ action: Selector, _ object: Any? = nil) -> NSMenuItem {
        let item = NSMenuItem(title: "", action: action, keyEquivalent: "")
        item.representedObject = object
        return item
    }

    // MARK: Undo and redo

    func testACellEditUndoesAndRedoesThroughTheWindowsUndoManager() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, undo, window) = (opened.model, opened.undo, opened.window)
        XCTAssertTrue(window.undoManager === undo, "⌘Z reaches the document's history")
        XCTAssertFalse(undo.canUndo)

        try set(model, 0, 1, "Marlowe")
        XCTAssertTrue(undo.canUndo)
        XCTAssertEqual(undo.undoMenuItemTitle, "Undo Typing")
        undo.undo()
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        XCTAssertEqual(undo.redoMenuItemTitle, "Redo Typing")
        undo.redo()
        XCTAssertEqual(value(model, 0, 1), "Marlowe")
        XCTAssertTrue(undo.canUndo)
        XCTAssertFalse(undo.canRedo)

        // Two edits are two steps, last first.
        try set(model, 1, 2, "6")
        undo.undo()
        XCTAssertEqual(value(model, 1, 2), "5")
        XCTAssertEqual(value(model, 0, 1), "Marlowe")
        undo.undo()
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        XCTAssertFalse(undo.canUndo)
        XCTAssertFalse(model.hasUnsavedEdits)

        // A new edit after an undo drops the redo history.
        undo.redo()
        try set(model, 2, 1, "Haldane")
        XCTAssertFalse(undo.canRedo)
    }

    func testABatchIsOneStep() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, undo) = (opened.model, opened.undo)
        try apply(model) {
            try $0.setCells(cells: [
                CellEdit(row: 1, column: 1, value: "A"),
                CellEdit(row: 2, column: 1, value: "B"),
                CellEdit(row: 3, column: 2, value: "C"),
            ])
        }
        XCTAssertEqual([value(model, 0, 1), value(model, 1, 1), value(model, 2, 2)], ["A", "B", "C"])
        XCTAssertEqual(undo.undoActionName, "Edit Cells")
        undo.undo()
        XCTAssertEqual([value(model, 0, 1), value(model, 1, 1), value(model, 2, 2)], ["Marlow", "Ostrava", "8"])
        XCTAssertFalse(undo.canUndo, "one step")
        undo.redo()
        XCTAssertEqual([value(model, 0, 1), value(model, 1, 1), value(model, 2, 2)], ["A", "B", "C"])
    }

    func testRowAndColumnCommandsUndoAndRedo() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, undo) = (opened.model, opened.undo)
        XCTAssertEqual(model.rowCount, 3)

        try apply(model) { try $0.deleteRows(at: 1, count: 2) }
        XCTAssertEqual(model.rowCount, 1)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Halden", truncated: false))
        XCTAssertEqual(undo.undoActionName, "Delete Rows")
        undo.undo()
        XCTAssertEqual(model.rowCount, 3)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Marlow", truncated: false))
        XCTAssertFalse(model.hasUnsavedEdits, "the rows' own bytes are back")
        undo.redo()
        XCTAssertEqual(model.rowCount, 1)
        undo.undo()

        try apply(model) { try $0.insertRows(at: 2, rows: [["9", "Vane", "1"]]) }
        XCTAssertEqual(undo.undoActionName, "Insert Row")
        XCTAssertEqual(model.cell(row: 1, column: 1), .text("Vane", truncated: false))
        undo.undo()
        XCTAssertEqual(model.cell(row: 1, column: 1), .text("Ostrava", truncated: false))

        try apply(model) { try $0.deleteColumn(at: 1) }
        XCTAssertEqual(undo.undoActionName, "Delete Column")
        XCTAssertEqual(model.columnCount, 2)
        XCTAssertEqual(model.headerTitle(column: 1).text, "qty")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("3", truncated: false))
        undo.undo()
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(model.headerTitle(column: 1).text, "name")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Marlow", truncated: false))
        XCTAssertEqual(model.columnWidths.count, 3)
        undo.redo()
        XCTAssertEqual(model.columnCount, 2)
        undo.undo()

        try apply(model) { try $0.insertColumn(at: 0, value: "x") }
        XCTAssertEqual(undo.undoActionName, "Insert Column")
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("x", truncated: false))
        undo.undo()
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("1", truncated: false))
        XCTAssertFalse(model.hasUnsavedEdits)
    }

    /// ⌘Z with an edit still open commits it first, then undoes it: the
    /// typing goes, and ⇧⌘Z brings it back.
    func testUndoCommitsAnOpenEditFirst() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        content.grid.activeCell = CellPosition(row: 0, column: 1)
        content.editActiveCell()
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        editor.selectAll(nil)
        editor.insertText("Typed", replacementRange: editor.selectedRange())
        undo.undo()
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        undo.redo()
        XCTAssertEqual(value(model, 0, 1), "Typed")
    }

    /// An undo the core refuses (the cell changed under it) clears the
    /// history, leaves the edits, and says so.
    func testARefusedUndoClearsTheHistory() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, undo) = (opened.document, opened.model, opened.undo)
        var sheets: [NSAlert] = []
        document.showSheet = { alert, _, _ in sheets.append(alert) }
        try set(model, 0, 1, "Marlowe")
        // Changed in the core alone, behind the history's back.
        _ = model.call { try $0.setCell(row: 1, column: 1, value: "Other") }
        undo.undo()
        XCTAssertFalse(undo.canUndo)
        XCTAssertFalse(undo.canRedo)
        XCTAssertEqual(value(model, 0, 1), "Other")
        XCTAssertEqual(sheets.first?.messageText, "Leal couldn’t undo “Typing”.")
    }

    /// An undo the core refuses only for now (a row delete undone while
    /// the Header row's new reading is still being read) is put back, to
    /// try again, and so is a redo; the history isn't cleared.
    func testAStepRefusedForNowIsPutBack() async throws {
        // Big enough (about 16 MB) that the new reading's index is still
        // running when the undo comes, straight after the toggle.
        var text = "id,name,qty\n"
        for row in 1...1_000_000 { text += "\(row),name \(row),\(row % 10)\n" }
        let opened = try await open(try file("big.csv", text))
        let (document, model, content, undo) = (opened.document, opened.model, opened.content, opened.undo)
        var sheets: [NSAlert] = []
        document.showSheet = { alert, _, _ in sheets.append(alert) }
        let readAgain = { model.call { try $0.progress() }?.complete == true }
        // The file's row 3 (logical, header row or not): "3" while it is
        // there, "4" once it is deleted.
        let row3 = { (model.call { try $0.fullValue(row: 3, column: 0) } ?? nil) ?? "" }
        try set(model, 0, 1, "first")
        try apply(model) { try $0.deleteRows(at: 3, count: 1) }
        XCTAssertEqual(row3(), "4")

        content.toggleHeaderRow(nil)
        undo.undo()
        XCTAssertEqual(row3(), "4", "refused: still reading")
        XCTAssertEqual(sheets.last?.messageText, "Leal couldn’t undo “Delete Row”.")
        XCTAssertEqual(sheets.last?.informativeText, "Leal is still reading the file. Try again in a moment.")
        XCTAssertTrue(undo.canUndo)
        XCTAssertEqual(undo.undoActionName, "Delete Row")
        try await waitUntil("read again", readAgain)
        undo.undo()
        XCTAssertEqual(row3(), "3")
        XCTAssertEqual(undo.undoActionName, "Typing", "the history before it is kept")

        content.toggleHeaderRow(nil)
        undo.redo()
        XCTAssertEqual(row3(), "3", "refused: still reading")
        XCTAssertEqual(sheets.last?.messageText, "Leal couldn’t redo “Delete Row”.")
        XCTAssertTrue(undo.canRedo)
        XCTAssertEqual(undo.redoActionName, "Delete Row")
        XCTAssertEqual(undo.undoActionName, "Typing")
        try await waitUntil("read again", readAgain)
        undo.redo()
        XCTAssertEqual(row3(), "4")
        XCTAssertEqual(sheets.count, 2)
        undo.undo()
        undo.undo()
        XCTAssertFalse(document.isDocumentEdited)
    }

    /// ⌘Z with an open edit the core refuses to commit: nothing is
    /// undone, and the edit stays open, saying why.
    func testUndoStopsWhenTheOpenEditIsRefused() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        try set(model, 0, 1, "Marlowe")
        content.grid.activeCell = CellPosition(row: 1, column: 1)
        content.editActiveCell()
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        editor.selectAll(nil)
        editor.insertText("Typed", replacementRange: editor.selectedRange())
        model.refusalForTesting = .valueChanged
        undo.undo()
        XCTAssertEqual(value(model, 0, 1), "Marlowe", "not undone")
        XCTAssertTrue(content.cellEditor.isEditing)
        XCTAssertTrue(undo.canUndo)
        undo.redo()
        XCTAssertTrue(content.cellEditor.isEditing)
        model.refusalForTesting = nil
        undo.undo()
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(value(model, 1, 1), "Ostrava")
        XCTAssertEqual(value(model, 0, 1), "Marlowe")
    }

    // MARK: Editing while the document is replaced

    /// While a Reload reads the file again, editing is off (the in-cell
    /// editor, the inspector, Rename Column) as undo is; an edit made all
    /// the same keeps the document, with its edits and history, and the
    /// user is told.
    func testEditingIsOffDuringAReloadAndAnEditMadeAllTheSameIsKept() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, content, undo) = (opened.document, opened.model, opened.content, opened.undo)
        var alerts: [NSAlert] = []
        content.showAlert = { alert, _ in alerts.append(alert) }
        content.setInspectorShown(true)
        content.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        XCTAssertNotNil(content.inspectorEdit)

        content.reloadFromDisk(nil)
        let reload = try XCTUnwrap(content.reloading)
        XCTAssertTrue(content.isReplacingDocument)
        content.editActiveCell()
        XCTAssertFalse(content.cellEditor.isEditing)
        content.rename(column: 1)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertNil(content.headerMenu(column: 1))
        let text = content.inspector.textView
        content.view.window?.makeFirstResponder(text)
        text.insertText("Typed", replacementRange: NSRange(location: 0, length: 0))
        XCTAssertEqual(text.string, "Marlow")
        XCTAssertEqual(content.inspectorEdit?.changed, false)

        // An edit made all the same, behind the window's back.
        try set(model, 0, 1, "Marlowe")
        XCTAssertFalse(undo.canUndo, "off while the file is read again")
        await reload.value
        XCTAssertEqual(value(model, 0, 1), "Marlowe")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(document.history.journal.count, 1)
        XCTAssertTrue(undo.canUndo)
        XCTAssertEqual(alerts.map(\.messageText), ["Leal didn’t reload “a.csv”."])
        XCTAssertFalse(content.isReplacingDocument)
        content.editActiveCell()
        XCTAssertTrue(content.cellEditor.isEditing)
    }

    // MARK: Quitting (task 2.5.2 review)

    /// Quit with one document whose only change is still being typed:
    /// `NSApplication.terminate` reviews the documents, which commits the
    /// edit first, so the document asks (and here cancels the quit).
    func testQuittingCommitsAnOpenEditThenAsks() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, content) = (opened.document, opened.model, opened.content)
        let controller = try XCTUnwrap(NSDocumentController.shared as? DocumentController)
        controller.addDocument(document)
        var asked = 0
        document.unsavedChangesPromptForTesting = { answer in
            asked += 1
            answer(false)
        }
        content.grid.activeCell = CellPosition(row: 0, column: 1)
        content.editActiveCell()
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        editor.selectAll(nil)
        editor.insertText("Typed", replacementRange: editor.selectedRange())
        XCTAssertFalse(controller.hasEditedDocuments, "not committed yet")

        let quit = QuitProbe()
        quit.terminate()
        try await waitUntil("reviewed") { !quit.reviewed.isEmpty }
        XCTAssertEqual(value(model, 0, 1), "Typed")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(asked, 1)
        XCTAssertEqual(quit.replies, [.terminateLater])
        XCTAssertEqual(quit.reviewed, [false], "Cancel: no quit")

        // Quit again: the document was edited before, so AppKit itself
        // reviews it, and the app delegate has nothing more to ask.
        quit.terminate()
        try await waitUntil("asked again") { asked == 2 }
        XCTAssertEqual(quit.replies, [.terminateLater], "the review cancelled the quit first")
    }

    /// As above with two documents, each with an edit still being typed:
    /// both are committed before AppKit counts the edited documents (its
    /// app-modal question is replaced here).
    func testQuittingCommitsEveryDocumentsOpenEdit() async throws {
        let first = try await open(try file("a.csv", csv))
        let second = try await open(try file("b.csv", csv))
        let controller = try XCTUnwrap(NSDocumentController.shared as? DocumentController)
        defer { controller.reviewForTesting = nil }
        for opened in [first, second] {
            controller.addDocument(opened.document)
            opened.content.grid.activeCell = CellPosition(row: 1, column: 2)
            opened.content.editActiveCell()
            let editor = try XCTUnwrap(opened.content.cellEditor.field.currentEditor() as? NSTextView)
            editor.selectAll(nil)
            editor.insertText("42", replacementRange: editor.selectedRange())
        }
        XCTAssertFalse(controller.hasEditedDocuments)
        var reviewed: [Bool] = []
        controller.reviewForTesting = { answer in
            reviewed = [first, second].map(\.document.isDocumentEdited)
            answer(false)
        }

        let quit = QuitProbe()
        quit.terminate()
        try await waitUntil("reviewed") { !quit.reviewed.isEmpty }
        XCTAssertEqual(reviewed, [true, true])
        XCTAssertEqual(value(first.model, 1, 2), "42")
        XCTAssertEqual(value(second.model, 1, 2), "42")
        XCTAssertEqual(quit.replies, [.terminateLater])
        XCTAssertEqual(quit.reviewed, [false])

        // AppKit's own review (Quit with edited documents) commits every
        // document's open edit before it counts them.
        second.content.grid.activeCell = CellPosition(row: 0, column: 2)
        second.content.editActiveCell()
        let editor = try XCTUnwrap(second.content.cellEditor.field.currentEditor() as? NSTextView)
        editor.selectAll(nil)
        editor.insertText("7", replacementRange: editor.selectedRange())
        let probe = ReviewProbe()
        controller.reviewUnsavedDocuments(
            withAlertTitle: nil,
            cancellable: true,
            delegate: probe,
            didReviewAllSelector: #selector(ReviewProbe.documentController(_:didReviewAll:contextInfo:)),
            contextInfo: nil
        )
        XCTAssertEqual(value(second.model, 0, 2), "7")
        XCTAssertEqual(probe.answers, [false])
    }

    // MARK: Dirty state

    func testTheChangeCountFollowsTheCoresDirtyState() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, undo, window) = (opened.document, opened.model, opened.undo, opened.window)
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertEqual(window.title, "a.csv")

        try set(model, 0, 1, "Marlowe")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertTrue(window.isDocumentEdited)
        XCTAssertEqual(window.title, "a.csv — Edited")
        XCTAssertNotNil(document.history.token(atVersion: model.editVersion))

        // Set back by hand: no cell reads differently, so it is clean.
        try set(model, 0, 1, "Marlow")
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertEqual(window.title, "a.csv")
        // Undoing that is an edit again; undoing the first, clean.
        undo.undo()
        XCTAssertTrue(document.isDocumentEdited)
        undo.undo()
        XCTAssertFalse(document.isDocumentEdited)
        undo.redo()
        XCTAssertTrue(document.isDocumentEdited)

        // Two edits, one undone: still edited.
        try set(model, 1, 2, "6")
        undo.undo()
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(document.history.journal.map(\.direction), [.edit, .edit, .undo, .undo, .redo, .edit, .undo])
    }

    /// The edited-cell triangles (mockup 05a): the cells the core names.
    func testEditedCellsAreMarked() async throws {
        let opened = try await open(try file("a.csv", csv))
        let model = opened.model
        try set(model, 1, 2, "6")
        _ = model.cell(row: 1, column: 2)
        XCTAssertTrue(model.isEdited(row: 1, column: 2))
        XCTAssertFalse(model.isEdited(row: 1, column: 1))
        XCTAssertFalse(model.isEdited(row: 0, column: 2))
        opened.undo.undo()
        _ = model.cell(row: 1, column: 2)
        XCTAssertFalse(model.isEdited(row: 1, column: 2))
    }

    // MARK: Re-reading with edits (ADR-0008 decision 4)

    func testTreatAsAndReopenWithEncodingAreOffWhileThereAreEdits() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (model, content, undo) = (opened.model, opened.content, opened.undo)
        let treatAs = menuItem(#selector(DocumentViewController.treatAsDelimiter(_:)), DelimiterBox(.semicolon))
        let reopen = menuItem(#selector(DocumentViewController.reopenWithEncoding(_:)), EncodingBox(.windows1252))
        let header = menuItem(#selector(DocumentViewController.toggleHeaderRow(_:)))
        XCTAssertTrue(content.validateMenuItem(treatAs))
        XCTAssertTrue(content.validateMenuItem(reopen))

        try set(model, 0, 1, "Marlowe")
        XCTAssertFalse(content.validateMenuItem(treatAs))
        XCTAssertEqual(treatAs.toolTip, "Save or revert your changes first.")
        XCTAssertFalse(content.validateMenuItem(reopen))
        XCTAssertEqual(reopen.toolTip, "Save or revert your changes first.")
        XCTAssertTrue(content.validateMenuItem(header), "the Header row stays")
        XCTAssertEqual(content.statusBar.delimiterButton?.isEnabled, false)
        XCTAssertEqual(content.statusBar.delimiterButton?.toolTip, "Save or revert your changes first.")
        XCTAssertEqual(content.statusBar.encodingButton?.isEnabled, false)
        content.treatAs(.semicolon)
        XCTAssertEqual(model.interpretation.delimiter, .comma)

        // The Header row keeps the edits and their history.
        content.toggleHeaderRow(nil)
        XCTAssertFalse(model.interpretation.header)
        XCTAssertTrue(undo.canUndo)
        undo.undo()
        XCTAssertEqual(model.fullValue(.cell(CellPosition(row: 1, column: 1))), "Marlow")
        XCTAssertTrue(content.validateMenuItem(treatAs))
        XCTAssertEqual(content.statusBar.delimiterButton?.isEnabled, true)

        // A new split: the commands from before it no longer apply.
        XCTAssertTrue(undo.canRedo)
        content.treatAs(.semicolon)
        XCTAssertEqual(model.interpretation.delimiter, .semicolon)
        XCTAssertFalse(undo.canUndo)
        XCTAssertFalse(undo.canRedo)
        XCTAssertTrue(opened.document.history.journal.isEmpty)
    }

    func testReloadAsksBeforeDiscardingEdits() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, content, undo) = (opened.document, opened.model, opened.content, opened.undo)
        var sheets: [(NSAlert, @MainActor (NSApplication.ModalResponse) -> Void)] = []
        document.showSheet = { alert, _, done in sheets.append((alert, done)) }
        try set(model, 0, 1, "Marlowe")

        content.reloadFromDisk(nil)
        let (alert, answer) = try XCTUnwrap(sheets.first)
        XCTAssertEqual(alert.messageText, "Reload “a.csv” and discard your changes?")
        XCTAssertEqual(alert.buttons.map(\.title), ["Reload", "Cancel"])
        XCTAssertNil(content.reloading)
        answer(.alertSecondButtonReturn)
        XCTAssertNil(content.reloading, "cancelled")
        XCTAssertEqual(value(model, 0, 1), "Marlowe")

        content.reloadFromDisk(nil)
        XCTAssertEqual(sheets.count, 2)
        sheets[1].1(.alertFirstButtonReturn)
        let reload = try XCTUnwrap(content.reloading)
        await reload.value
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertFalse(undo.canUndo)
        XCTAssertTrue(document.history.journal.isEmpty)

        // With no edits, Reload doesn't ask.
        content.reloadFromDisk(nil)
        XCTAssertEqual(sheets.count, 2)
        await content.reloading?.value
    }

    /// Closing commits an open edit, so it counts, then asks.
    func testCloseCommitsAnOpenEditThenAsks() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, content) = (opened.document, opened.model, opened.content)
        var asked = 0
        document.unsavedChangesPromptForTesting = { answer in
            asked += 1
            answer(false)
        }
        let probe = CloseProbe()
        document.canClose(withDelegate: probe, shouldClose: #selector(CloseProbe.document(_:shouldClose:contextInfo:)), contextInfo: nil)
        XCTAssertEqual(probe.answers, [true], "nothing to save")
        XCTAssertEqual(asked, 0)

        content.grid.activeCell = CellPosition(row: 0, column: 1)
        content.editActiveCell()
        let editor = try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView)
        editor.selectAll(nil)
        editor.insertText("Typed", replacementRange: editor.selectedRange())
        XCTAssertFalse(document.isDocumentEdited, "not committed yet")

        document.canClose(withDelegate: probe, shouldClose: #selector(CloseProbe.document(_:shouldClose:contextInfo:)), contextInfo: nil)
        XCTAssertEqual(value(model, 0, 1), "Typed")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(asked, 1)
        XCTAssertEqual(probe.answers, [true, false])
    }

    // MARK: Recover changes (ADR-0008 decision 5)

    func testRecoverChangesReplaysTheJournalAfterAFailure() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, undo) = (opened.document, opened.model, opened.undo)
        var sheets: [NSAlert] = []
        document.isOnScreen = { _ in true }
        document.showSheet = { alert, _, _ in sheets.append(alert) }
        try set(model, 0, 1, "Marlowe")
        try set(model, 1, 1, "Ostravo")
        undo.undo()
        undo.redo()
        try set(model, 2, 1, "Haldane")
        undo.undo()
        try apply(model) { try $0.deleteRows(at: 3, count: 1) }
        try apply(model) { try $0.insertColumn(at: 3, value: "new") }
        try set(model, 0, 3, "first")
        let expected = (0..<2).map { row in (0..<4).map { value(model, row, $0) } }
        XCTAssertEqual(expected[1], ["2", "Ostravo", "5", "new"])

        _ = model.call { try $0.debugPanic() }
        XCTAssertTrue(model.isFailed)
        XCTAssertFalse(undo.canUndo, "the history waits for Recover changes")
        let alert = try XCTUnwrap(sheets.first)
        XCTAssertEqual(alert.buttons.map(\.title), ["Recover Changes", "Reopen", "Close"])

        await document.recoverChanges()
        XCTAssertFalse(model.isFailed)
        let report = try XCTUnwrap(document.lastRecovery)
        XCTAssertTrue(report.refused.isEmpty, "\(report.refused)")
        XCTAssertEqual(model.rowCount, 2)
        XCTAssertEqual((0..<2).map { row in (0..<4).map { value(model, row, $0) } }, expected)
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(sheets.count, 1, "the file is unchanged: no Save As offered")
        XCTAssertFalse(document.isOfferingReopen)

        // The steps still done are the new undo history, last first.
        XCTAssertEqual(undo.undoActionName, "Typing")
        undo.undo()
        XCTAssertEqual(value(model, 0, 3), "new")
        undo.undo()
        XCTAssertEqual(model.columnCount, 3)
        undo.undo()
        XCTAssertEqual(model.rowCount, 3)
        undo.undo()
        undo.undo()
        XCTAssertFalse(undo.canUndo)
        XCTAssertFalse(model.hasUnsavedEdits)
        XCTAssertFalse(document.isDocumentEdited)

        // A second failure recovers the recovered history's journal.
        undo.redo()
        _ = model.call { try $0.debugPanic() }
        await document.recoverChanges()
        XCTAssertEqual(value(model, 0, 1), "Marlowe")
        XCTAssertEqual(value(model, 1, 1), "Ostrava")
    }

    func testRecoverChangesNamesTheEditsThatNoLongerApply() async throws {
        let url = try file("a.csv", csv)
        let opened = try await open(url)
        let (document, model) = (opened.document, opened.model)
        var sheets: [(NSAlert, @MainActor (NSApplication.ModalResponse) -> Void)] = []
        document.isOnScreen = { _ in true }
        document.showSheet = { alert, _, done in sheets.append((alert, done)) }
        try set(model, 0, 1, "Marlowe")
        try set(model, 2, 2, "9")
        _ = model.call { try $0.debugPanic() }

        // Another app changes row 1 meanwhile.
        try Data("id,name,qty\n1,Marlow & Co,3\n2,Ostrava,5\n3,Halden,8\n".utf8).write(to: url)
        try FileManager.default.setAttributes([.modificationDate: Date().addingTimeInterval(60)], ofItemAtPath: url.path(percentEncoded: false))
        await document.recoverChanges()
        let report = try XCTUnwrap(document.lastRecovery)
        XCTAssertEqual(report.refused.map(\.index), [0])
        XCTAssertEqual(report.refused.map(\.refusal), [.valueChanged])
        XCTAssertEqual(value(model, 0, 1), "Marlow & Co")
        XCTAssertEqual(value(model, 2, 2), "9")

        let (alert, _) = try XCTUnwrap(sheets.last)
        XCTAssertEqual(alert.messageText, "Leal put back 1 of your 2 changes to “a.csv”.")
        XCTAssertTrue(alert.informativeText.hasPrefix("• Row 1, column 2: the cell changed"), alert.informativeText)
        XCTAssertEqual(alert.buttons.map(\.title), ["Save As…", "Not Now"])
        XCTAssertEqual(opened.undo.undoActionName, "Typing")
        XCTAssertEqual(document.history.journal.count, 1)
    }

    /// The words for edits Recover changes couldn't put back count in the
    /// right number (the String Catalog's plural variants).
    func testRecoveryWordsCountInTheRightNumber() async throws {
        let opened = try await open(try file("a.csv", csv))
        let model = opened.model
        func describe(_ make: (LealFFI.Document) throws -> EditCommand?) throws -> String {
            HistoryText.describe(try XCTUnwrap(model.call { try make($0) } ?? nil), header: true)
        }
        XCTAssertEqual(try describe { try $0.insertRows(at: 2, rows: [["9", "Vane", "1"]]) }, "Inserting 1 row at Row 2")
        XCTAssertEqual(try describe { try $0.insertRows(at: 1, rows: [["7", "a", "1"], ["8", "b", "2"]]) }, "Inserting 2 rows at Row 1")
        XCTAssertEqual(try describe { try $0.deleteRows(at: 3, count: 1) }, "Deleting 1 row at Row 3")
        XCTAssertEqual(try describe { try $0.deleteRows(at: 1, count: 2) }, "Deleting 2 rows at Row 1")
        XCTAssertEqual(
            try describe { try $0.setCells(cells: [CellEdit(row: 1, column: 1, value: "A"), CellEdit(row: 2, column: 1, value: "B")]) },
            "Row 1, column 2 and 1 more cell"
        )
        XCTAssertEqual(
            try describe {
                try $0.setCells(cells: [
                    CellEdit(row: 1, column: 1, value: "C"),
                    CellEdit(row: 2, column: 1, value: "D"),
                    CellEdit(row: 2, column: 2, value: "E"),
                ])
            },
            "Row 1, column 2 and 2 more cells"
        )
    }

    /// Recover Changes is offered while the journal has commands; then
    /// the alert says Reopen and Close discard the edits.
    func testAFailureWithNoEditsOffersNoRecovery() async throws {
        let opened = try await open(try file("a.csv", csv))
        let (document, model, undo) = (opened.document, opened.model, opened.undo)
        XCTAssertFalse(document.canRecover, "no edits")
        let plain = document.failureAlert()
        XCTAssertEqual(plain.buttons.map(\.title), ["Reopen", "Close"])
        XCTAssertFalse(plain.buttons.contains(where: \.hasDestructiveAction))
        try set(model, 0, 1, "Marlowe")
        undo.undo()
        XCTAssertTrue(document.canRecover, "the journal, not the dirty state")
        let alert = document.failureAlert()
        XCTAssertEqual(alert.buttons.map(\.title), ["Recover Changes", "Reopen", "Close"])
        XCTAssertTrue(alert.informativeText.hasSuffix("Recover Changes opens the file again and puts your unsaved edits back. Reopen and Close discard them."), alert.informativeText)
        XCTAssertEqual(alert.buttons.map(\.hasDestructiveAction), [false, true, true])
    }
}

/// Quits through `NSApplication.terminate`, as ⌘Q does, with an app
/// delegate in place of Leal's. It asks the document controller as Leal's
/// does, but hears the answer itself, and never lets the app quit, so a
/// review that doesn't stop the quit can't end the test host.
@MainActor
private final class QuitProbe: NSObject, NSApplicationDelegate {
    /// What `DocumentController.shouldTerminate` returned.
    private(set) var replies: [NSApplication.TerminateReply] = []
    /// What its review answered later.
    private(set) var reviewed: [Bool] = []

    func terminate() {
        let saved = NSApp.delegate
        NSApp.delegate = self
        defer { NSApp.delegate = saved }
        NSApp.terminate(nil)
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let controller = NSDocumentController.shared as? DocumentController
        if let reply = controller?.shouldTerminate(reply: { [weak self] quit in self?.reviewed.append(quit) }) {
            replies.append(reply)
        }
        return .terminateCancel
    }
}

/// `reviewUnsavedDocuments`'s delegate, which hears the answer.
@MainActor
private final class ReviewProbe: NSObject {
    private(set) var answers: [Bool] = []

    @objc func documentController(_ controller: NSDocumentController, didReviewAll: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(didReviewAll)
    }
}

/// `canClose`'s delegate, which hears the answer.
@MainActor
private final class CloseProbe: NSObject {
    private(set) var answers: [Bool] = []

    @objc func document(_ document: NSDocument, shouldClose: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(shouldClose)
    }
}
