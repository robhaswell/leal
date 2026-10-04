import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.5.1 against the real core, hosted in Leal.app: the in-cell
/// editor (Return, typing, Tab, Esc, leaving it), the header row's editor,
/// the inspector's editing, `canEdit`, the callouts (mockup 05b), the
/// encoding check before a commit, UTF-16, and what an edit brings up to
/// date: the grid, the marks, widths and number detection, Find (catching
/// up), Copy and the inspector (ADR-0008 decisions 2 and 3).
///
/// The views are driven directly: key events made here go to the views
/// themselves, never to the system (CLAUDE.md).
@MainActor
final class EditingTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?
    private var savedDelay: Duration = .zero

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-edit-\(UUID().uuidString)")
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
        savedDelay = DocumentModel.sizingAfterEditDelay
        DocumentModel.sizingAfterEditDelay = .milliseconds(1)
    }

    override func tearDown() async throws {
        DocumentModel.sizingAfterEditDelay = savedDelay
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private func file(_ name: String, _ bytes: Data) throws -> URL {
        let url = directory.appending(path: name)
        try bytes.write(to: url)
        return url
    }

    private func file(_ name: String, _ text: String) throws -> URL {
        try file(name, Data(text.utf8))
    }

    private func open(_ url: URL) async throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        content.pasteboard = NSPasteboard(name: NSPasteboard.Name("io.github.robhaswell.leal.tests.\(UUID().uuidString)"))
        addTeardownBlock { @MainActor in content.pasteboard.releaseGlobally() }
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        return (document, model, content)
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

    /// The in-cell editor's text view (the window's field editor).
    private func fieldEditor(_ content: DocumentViewController) throws -> NSTextView {
        try XCTUnwrap(content.cellEditor.field.currentEditor() as? NSTextView, "the editor isn't open")
    }

    /// Replaces the editor's text as typing would (so the editor hears of
    /// the change).
    private func type(_ text: String, in content: DocumentViewController) throws {
        let editor = try fieldEditor(content)
        editor.selectAll(nil)
        editor.insertText(text, replacementRange: editor.selectedRange())
    }

    /// A key binding's command in the editor: Return, Tab, Esc.
    private func press(_ selector: Selector, in content: DocumentViewController) throws {
        try fieldEditor(content).doCommand(by: selector)
    }

    private func key(_ characters: String, code: UInt16 = 0, flags: NSEvent.ModifierFlags = [], window: NSWindow?) throws -> NSEvent {
        try XCTUnwrap(NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: window?.windowNumber ?? 0,
            context: nil, characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code
        ))
    }

    private func value(_ model: DocumentModel, _ row: Int, _ column: Int) -> String? {
        model.fullValue(.cell(CellPosition(row: row, column: column)))
    }

    private func editedCells(_ model: DocumentModel) -> UInt64 {
        model.call { try $0.editedCellCount() } ?? 0
    }

    /// Opens the editor on `cell` with Return and waits for a long value
    /// to be read.
    private func returnKey(on cell: CellPosition, _ content: DocumentViewController) async throws {
        content.grid.select(cell)
        content.grid.gridView.insertNewline(nil)
        await content.cellEditor.loading?.value
        XCTAssertTrue(content.cellEditor.isEditing, "the editor didn't open on \(cell)")
    }

    // MARK: The in-cell editor

    func testReturnEditsFromTheFullValueAndCommits() async throws {
        let long = String(repeating: "abcdefghij", count: 30)
        let (_, model, content) = try await open(try file("edit.csv", "id,name,notes\n1,Sable,\"\(long)\"\n2,Loire,\"Deliver to the rear.\nCall on arrival.\"\n"))
        let grid = content.grid

        // A long value: the grid shows its start, the editor all of it.
        guard case let .text(shown, truncated: true) = model.cell(row: 0, column: 2) else { return XCTFail("not cut short") }
        XCTAssertLessThan(shown.count, long.count)
        try await returnKey(on: CellPosition(row: 0, column: 2), content)
        XCTAssertEqual(content.cellEditor.field.stringValue, long)
        XCTAssertTrue(content.cellEditor.field.superview === grid.overlay, "the editor is in the overlay, above the strips")
        try press(#selector(NSResponder.cancelOperation(_:)), in: content)

        // A multiline value: its line breaks, not the grid's ↵.
        try await returnKey(on: CellPosition(row: 1, column: 2), content)
        XCTAssertEqual(content.cellEditor.field.stringValue, "Deliver to the rear.\nCall on arrival.")
        XCTAssertGreaterThan(content.cellEditor.field.frame.height, GridMetrics.rowHeight, "two lines show")
        try press(#selector(NSResponder.cancelOperation(_:)), in: content)

        // Return commits: the grid, the core and the inspector read it.
        content.setInspectorShown(true)
        try await returnKey(on: CellPosition(row: 0, column: 1), content)
        XCTAssertEqual(content.cellEditor.field.stringValue, "Sable")
        try type("Sable Optics", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertNil(content.cellEditor.field.superview)
        XCTAssertTrue(content.view.window?.firstResponder === grid.gridView)
        XCTAssertEqual(grid.activeCell, CellPosition(row: 0, column: 1), "Return stays on the cell")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Sable Optics", truncated: false))
        XCTAssertEqual(value(model, 0, 1), "Sable Optics")
        XCTAssertEqual(editedCells(model), 1)
        await content.inspectorTask?.value
        XCTAssertEqual(content.inspector.textView.string, "Sable Optics")
    }

    /// ADR-0008 decision 3: committing an untouched value is no edit, for
    /// a long and a multiline value, in the editor and the inspector.
    func testUntouchedLongAndMultilineValuesAreNoEdit() async throws {
        let long = String(repeating: "0123456789", count: 7_000)
        // The long row second: the header row is guessed from the first 64 KB.
        let (_, model, content) = try await open(try file("untouched.csv", "id,notes\n1,\"one\r\ntwo\rthree\"\n2,\"\(long)\"\n"))
        XCTAssertTrue(model.interpretation.header)
        for row in 0..<2 {
            try await returnKey(on: CellPosition(row: row, column: 1), content)
            try press(#selector(NSResponder.insertNewline(_:)), in: content)
            XCTAssertFalse(content.cellEditor.isEditing)
            // Tab and leaving the editor commit too.
            try await returnKey(on: CellPosition(row: row, column: 1), content)
            try press(#selector(NSResponder.insertTab(_:)), in: content)
            try await returnKey(on: CellPosition(row: row, column: 1), content)
            content.view.window?.makeFirstResponder(content.grid.gridView)
            XCTAssertFalse(content.cellEditor.isEditing)
        }
        XCTAssertEqual(editedCells(model), 0)
        XCTAssertEqual(model.call { try $0.hasUnsavedEdits() }, false)
        XCTAssertEqual(value(model, 0, 1), "one\r\ntwo\rthree")

        // The inspector: the long value is cut at 64,000 characters, and
        // read in full before it can be edited.
        content.setInspectorShown(true)
        content.grid.select(CellPosition(row: 1, column: 1))
        await content.inspectorTask?.value
        let text = content.inspector.textView
        XCTAssertTrue(text.isEditable)
        XCTAssertEqual(content.inspectorEdit?.truncated, true)
        XCTAssertFalse(text.shouldChangeText(in: NSRange(location: 0, length: 0), replacementString: "x"), "the start isn't edited")
        await content.inspectorLoading?.value
        XCTAssertEqual(content.inspectorEdit?.truncated, false)
        XCTAssertEqual(text.string, long)
        content.commitInspector(refocus: true)
        content.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        XCTAssertEqual(text.string, "one\r\ntwo\rthree")
        content.commitInspector(refocus: true)
        XCTAssertEqual(editedCells(model), 0)
    }

    func testEscCancelsTabMovesOnAndLeavingCommits() async throws {
        let (_, model, content) = try await open(try file("keys.csv", "a,b,c\n1,2,3\n4,5,6\n"))
        try await returnKey(on: CellPosition(row: 0, column: 0), content)
        try type("changed", in: content)
        try press(#selector(NSResponder.cancelOperation(_:)), in: content)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(value(model, 0, 0), "1")

        // Tab commits and moves along the row, Shift-Tab back.
        try await returnKey(on: CellPosition(row: 0, column: 0), content)
        try type("one", in: content)
        try press(#selector(NSResponder.insertTab(_:)), in: content)
        XCTAssertEqual(value(model, 0, 0), "one")
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 0, column: 1))
        try await returnKey(on: CellPosition(row: 0, column: 2), content)
        try press(#selector(NSResponder.insertBacktab(_:)), in: content)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 0, column: 1))

        // A click elsewhere (the grid taking the focus) commits.
        try await returnKey(on: CellPosition(row: 1, column: 2), content)
        try type("six", in: content)
        content.view.window?.makeFirstResponder(content.grid.gridView)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(value(model, 1, 2), "six")
        XCTAssertEqual(editedCells(model), 2)
    }

    /// DESIGN §4.2: typing edits the active cell, replacing its value; the
    /// key goes to the editor (so an input method could compose from it).
    func testTypingStartsAnEdit() async throws {
        let (_, model, content) = try await open(try file("typing.csv", "a,b\nold,2\n"))
        let window = content.view.window
        XCTAssertTrue(GridView.typesText(try key("x", window: window)))
        XCTAssertTrue(GridView.typesText(try key("É", flags: .shift, window: window)))
        XCTAssertFalse(GridView.typesText(try key("c", flags: .command, window: window)))
        XCTAssertFalse(GridView.typesText(try key("\t", code: 48, window: window)))
        XCTAssertFalse(GridView.typesText(try key("\r", code: 36, window: window)))
        XCTAssertFalse(GridView.typesText(try key("\u{F700}", code: 126, flags: [.numericPad, .function], window: window)))

        content.grid.select(CellPosition(row: 0, column: 0))
        content.view.window?.makeFirstResponder(content.grid.gridView)
        content.grid.gridView.keyDown(with: try key("n", code: 45, window: window))
        XCTAssertTrue(content.cellEditor.isEditing)
        XCTAssertEqual(content.cellEditor.field.stringValue, "n")
        try fieldEditor(content).insertText("ew", replacementRange: NSRange(location: NSNotFound, length: 0))
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(value(model, 0, 0), "new")
    }

    /// ADR-0005 decision 2: a short row's hatched cell can be edited; it
    /// is no longer missing, and setting it back to "" makes it missing
    /// again.
    func testHatchedCellsCanBeEdited() async throws {
        let (_, model, content) = try await open(try file("short.csv", "a,b,c\n1,2,3\n4\n5,6,7\n"))
        XCTAssertEqual(model.cell(row: 1, column: 2), .missing)
        XCTAssertTrue(model.isHatched(row: 1, column: 2))
        try await returnKey(on: CellPosition(row: 1, column: 2), content)
        XCTAssertEqual(content.cellEditor.field.stringValue, "")
        try type("filled", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(model.cell(row: 1, column: 2), .text("filled", truncated: false))
        XCTAssertEqual(model.cell(row: 1, column: 1), .text("", truncated: false), "the cells before it are fields now")
        try await returnKey(on: CellPosition(row: 1, column: 2), content)
        try type("", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(model.cell(row: 1, column: 2), .missing)
        XCTAssertEqual(editedCells(model), 0)
    }

    // MARK: The header row

    /// docs/tasks/2.1.md: a header-row cell is edited in place ("Rename
    /// Column…"), from its full value; the titles follow; Find still
    /// doesn't search it; a hatched header cell can be edited too.
    func testHeaderCellsAreEditedInPlace() async throws {
        let (_, model, content) = try await open(try file("header.csv", "id,\"first\nname\"\n1,Ada,x\n2,Bo,y\n"))
        XCTAssertTrue(model.interpretation.header)
        let menu = try XCTUnwrap(content.headerMenu(column: 1))
        XCTAssertEqual(menu.items.map(\.title), ["Rename Column…"])

        content.rename(column: 1)
        XCTAssertTrue(content.cellEditor.isEditing)
        XCTAssertTrue(content.cellEditor.field.superview === content.grid.headerView)
        XCTAssertEqual(content.cellEditor.field.stringValue, "first\nname", "the full value, not the title's ↵")
        try type("customer", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(model.headerTitle(column: 1), HeaderTitle(text: "customer", style: .name))
        XCTAssertEqual(model.fullValue(.header(column: 1)), "customer")
        XCTAssertEqual(value(model, 0, 1), "Ada", "grid row 0 is unchanged")

        // Find doesn't search the header row.
        content.showFindBar()
        content.findBar.field.stringValue = "customer"
        content.search(for: "customer")
        try await waitUntil("searched") { !content.find.isSearching }
        XCTAssertEqual(content.find.matchCount, 0)
        content.hideFindBar()

        // The header is shorter than the rows: its third cell is hatched.
        XCTAssertEqual(model.headerTitle(column: 2).style, .extra)
        content.rename(column: 2)
        XCTAssertEqual(content.cellEditor.field.stringValue, "")
        try type("flag", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(model.headerTitle(column: 2), HeaderTitle(text: "flag", style: .name))
        XCTAssertEqual(editedCells(model), 2)
    }

    // MARK: Edits are visible everywhere (ADR-0008 decision 2)

    /// A hosted test edits a cell, then finds it, copies it and steps to it.
    func testAnEditIsFoundCopiedAndSteppedTo() async throws {
        let (_, model, content) = try await open(try file("everywhere.csv", "id,customer\n1,Marlow\n2,Ostrava\n3,Marlow\n"))
        content.showFindBar()
        content.findBar.field.stringValue = "marlow"
        content.search(for: "marlow")
        try await waitUntil("searched") { !content.find.isSearching && content.find.pendingStep == nil }
        XCTAssertEqual(content.find.matchCount, 2)

        // Ostrava becomes a Marlow: three matches, and the old value none.
        try await returnKey(on: CellPosition(row: 1, column: 1), content)
        try type("Marlow & Co", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        try await waitUntil("caught up") { content.find.progress?.catchingUp == false }
        XCTAssertEqual(content.find.matchCount, 3)
        XCTAssertEqual(content.find.current?.cell, CellPosition(row: 1, column: 1), "the edited cell is a match now")
        XCTAssertNotNil(content.find.highlight(row: 1, column: 1))

        // Steps reach it.
        content.grid.select(CellPosition(row: 0, column: 1))
        content.step(forward: true)
        try await waitUntil("stepped") { content.find.pendingStep == nil }
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 1, column: 1))
        XCTAssertEqual(content.find.current?.ordinal, 2)

        // And the old value isn't found.
        content.findBar.field.stringValue = "ostrava"
        content.search(for: "ostrava")
        try await waitUntil("searched") { !content.find.isSearching && content.find.pendingStep == nil }
        XCTAssertEqual(content.find.matchCount, 0)

        // Copy has the edited value.
        content.grid.select(CellPosition(row: 1, column: 1))
        content.grid.extend(to: CellPosition(row: 1, column: 0))
        content.copySelection()
        XCTAssertEqual(content.pasteboard.string(forType: .string), "2\tMarlow & Co")
        XCTAssertEqual(value(model, 1, 1), "Marlow & Co")
    }

    /// Find catches up with an edit in a job of its own when there are many
    /// matches (`catchingUp`), and a Next waiting for it (`Pending`) is
    /// answered once it has.
    func testFindCatchesUpWithAnEditAndRetriesAWaitingStep() async throws {
        var text = "id,tag\n"
        for row in 0..<70_000 { text += "\(row),x\n" }
        let (_, model, content) = try await open(try file("many.csv", text))
        content.showFindBar()
        content.findBar.field.stringValue = "x"
        content.search(for: "x")
        try await waitUntil("searched") { !content.find.isSearching && content.find.pendingStep == nil }
        XCTAssertEqual(content.find.matchCount, 70_000)

        // The edit removes a match; a Next from the cell before it asks at
        // once, while the search may still be catching up.
        content.grid.select(CellPosition(row: 50_000, column: 1))
        try await returnKey(on: CellPosition(row: 50_000, column: 1), content)
        try type("y", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        content.grid.select(CellPosition(row: 49_999, column: 1))
        content.step(forward: true)
        try await waitUntil("caught up and stepped") {
            content.find.progress?.catchingUp == false && content.find.pendingStep == nil
        }
        XCTAssertEqual(content.find.matchCount, 69_999)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 50_001, column: 1), "the edited cell is no match")
        XCTAssertEqual(value(model, 50_000, 1), "y")
    }

    /// An edit redraws its row, its gutter mark and its column: the
    /// invalid bytes' marker goes, a column widens for a wider value, and
    /// number detection is done again.
    func testAnEditRefreshesMarksWidthsAndNumbers() async throws {
        var bytes = Data("\u{FEFF}id,qty,name\n1,40,Caf".utf8)
        bytes.append(0xE9)
        bytes.append(Data(" Marlow\n2,3,Bo\n3,12,Cy\n".utf8))
        let (_, model, content) = try await open(try file("marks.csv", bytes))
        try await waitUntil("sized") { model.isSizingRefined }
        XCTAssertTrue(model.rowHasMarker(0))
        XCTAssertTrue(model.isNumeric(column: 1))

        // The invalid bytes: the callout, and the marker gone once replaced.
        try await returnKey(on: CellPosition(row: 0, column: 2), content)
        XCTAssertEqual(content.cellEditor.field.stringValue, "Caf\u{FFFD} Marlow")
        XCTAssertTrue(content.cellEditor.callout.superview === content.grid.overlay)
        XCTAssertTrue(content.cellEditor.callout.message.contains("aren’t valid UTF-8"))
        try type("Café Marlow", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertNil(content.cellEditor.callout.superview)
        XCTAssertFalse(model.rowHasMarker(0))

        // A wider value widens its column.
        let before = model.columnWidths[2]
        try await returnKey(on: CellPosition(row: 1, column: 2), content)
        try type("A much longer customer name than any other", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertGreaterThan(model.columnWidths[2], before)
        XCTAssertEqual(content.grid.geometry.widths[2], model.columnWidths[2])

        // Text in a number column: measured again, it isn't one.
        try await returnKey(on: CellPosition(row: 2, column: 1), content)
        try type("twelve", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        try await waitUntil("measured again") { !model.isMeasuringAfterEdit }
        XCTAssertFalse(model.isNumeric(column: 1))
    }

    // MARK: Where an edit can't be made, or can't be saved

    /// The editor opens only where `canEdit` allows: a cell after an
    /// unterminated quote is refused, and the callout says why.
    func testTheEditorOpensOnlyWhereTheCoreAllows() async throws {
        let (_, model, content) = try await open(try file("quote.csv", "a,b,c\n1,2,3\n4,\"open,6\n7,8,9\n"))
        let cell = CellPosition(row: 1, column: 2)
        XCTAssertEqual(model.editRefusal(.cell(cell)), .afterUnterminatedQuote)
        content.grid.select(cell)
        content.grid.gridView.insertNewline(nil)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(content.cellEditor.shownRefusal, .afterUnterminatedQuote)
        XCTAssertTrue(content.cellEditor.callout.superview === content.grid.overlay)
        content.grid.select(CellPosition(row: 0, column: 0))
        XCTAssertNil(content.cellEditor.shownRefusal)
        XCTAssertNil(content.cellEditor.callout.superview)
        XCTAssertEqual(editedCells(model), 0)
    }

    /// docs/tasks/2.3.md: a character the encoding can't hold is named
    /// before the edit is committed. As it is typed, the callout names it
    /// and Return commits; a value too long to check as it is typed is
    /// checked at Return, which commits only when pressed again.
    func testUnencodableCharactersAreNamedBeforeCommitting() async throws {
        let (_, model, content) = try await open(try file("viet.csv", "a,b\nPho,1\nBun,2\n"))
        model.reopen(encoding: .windows1258)
        try await waitUntil("read again") { model.isIndexComplete && model.interpretation.encoding == .windows1258 }

        try await returnKey(on: CellPosition(row: 0, column: 0), content)
        try type("Pho ế", in: content)
        let callout = content.cellEditor.callout
        XCTAssertTrue(callout.superview === content.grid.overlay)
        XCTAssertTrue(callout.message.contains("“ế” can’t be saved in Windows-1258"), callout.message)
        XCTAssertTrue(callout.message.contains("Save As UTF-8"))
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertFalse(content.cellEditor.isEditing, "named while typing: Return commits")
        XCTAssertEqual(value(model, 0, 0), "Pho ế")

        let long = String(repeating: "a", count: CellEditController.liveCheckLimit) + "ế"
        try await returnKey(on: CellPosition(row: 1, column: 0), content)
        try type(long, in: content)
        XCTAssertNil(callout.superview, "too long to check as it is typed")
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertTrue(content.cellEditor.isEditing, "the first Return names it")
        XCTAssertTrue(callout.message.contains("“ế”"))
        XCTAssertEqual(value(model, 1, 0), "Bun")
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertEqual(value(model, 1, 0), long)
    }

    /// ADR-0013 decision 1: a UTF-16 file can be edited; Save is off.
    func testUTF16FilesAreEditableWithSaveOff() async throws {
        var data = Data([0xFF, 0xFE])
        data.append("id\tname\r\n1\tZoë\r\n".data(using: .utf16LittleEndian)!)
        let (document, model, content) = try await open(try file("wide.tsv", data))
        XCTAssertTrue(model.isReadOnly)
        XCTAssertFalse(document.canSave)
        XCTAssertNil(model.editRefusal(.cell(CellPosition(row: 0, column: 1))))
        try await returnKey(on: CellPosition(row: 0, column: 1), content)
        try type("Zoë Ångström", in: content)
        try press(#selector(NSResponder.insertNewline(_:)), in: content)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Zoë Ångström", truncated: false))
    }

    // MARK: The inspector (mockup 05a)

    func testTheInspectorEditsAMultilineValue() async throws {
        let (_, model, content) = try await open(try file("notes.csv", "id,notes\n12,\"Deliver to the rear entrance.\nCall on arrival.\"\n13,x\n"))
        content.setInspectorShown(true)
        content.grid.select(CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        let text = content.inspector.textView
        XCTAssertTrue(text.isEditable)
        XCTAssertTrue(content.inspector.sizeLabel.stringValue.hasSuffix("⌘↩ commits · Esc cancels"))
        content.view.window?.makeFirstResponder(text)
        text.setSelectedRange(NSRange(location: (text.string as NSString).length, length: 0))
        text.insertText("\nGate code 4471", replacementRange: text.selectedRange())
        XCTAssertEqual(content.inspectorEdit?.changed, true)

        // Esc puts the value back; ⌘↩ commits.
        text.cancelOperation(nil)
        await content.inspectorTask?.value
        XCTAssertEqual(text.string, "Deliver to the rear entrance.\nCall on arrival.")
        content.view.window?.makeFirstResponder(text)
        text.setSelectedRange(NSRange(location: (text.string as NSString).length, length: 0))
        text.insertText("\nGate code 4471", replacementRange: text.selectedRange())
        text.onCommit?()
        XCTAssertEqual(value(model, 0, 1), "Deliver to the rear entrance.\nCall on arrival.\nGate code 4471")
        XCTAssertTrue(content.view.window?.firstResponder === content.grid.gridView)
        await content.inspectorTask?.value
        XCTAssertEqual(text.string, "Deliver to the rear entrance.\nCall on arrival.\nGate code 4471")

        // Moving on commits an edit not yet committed.
        content.view.window?.makeFirstResponder(text)
        text.insertText("!", replacementRange: NSRange(location: (text.string as NSString).length, length: 0))
        content.grid.select(CellPosition(row: 1, column: 1))
        XCTAssertEqual(value(model, 0, 1), "Deliver to the rear entrance.\nCall on arrival.\nGate code 4471!")
    }

    // MARK: Cell edit to screen (DESIGN §1, < 16 ms)

    /// The time from Return to the transaction that draws the edit, for
    /// the record (`just perf` measures the Release app's; a Debug test
    /// build is slower, so this only checks it is measured).
    func testCellEditToScreenIsMeasured() async throws {
        var text = "id,name,qty\n"
        for row in 0..<5_000 { text += "\(row),name \(row),\(row % 97)\n" }
        let (_, _, content) = try await open(try file("timing.csv", text))
        content.view.window?.setContentSize(NSSize(width: 1000, height: 700))
        content.view.layoutSubtreeIfNeeded()
        content.view.displayIfNeeded()
        var times: [Double] = []
        content.cellEditor.onEditOnScreen = { times.append($0) }
        for row in 0..<10 {
            try await returnKey(on: CellPosition(row: row, column: 1), content)
            try type("edited \(row)", in: content)
            try press(#selector(NSResponder.insertNewline(_:)), in: content)
            CATransaction.flush()
            try await waitUntil("on screen") { times.count == row + 1 }
        }
        let sorted = times.sorted()
        let median = sorted[sorted.count / 2]
        XCTContext.runActivity(named: "Cell edit to screen: median \(median) ms, max \(sorted.last ?? 0) ms") { _ in }
        XCTAssertLessThan(median, 100)
    }
}
