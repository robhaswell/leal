import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 1.8 against the real core, hosted in Leal.app: find (counts,
/// Next and Previous at the edges, highlights, while indexing), Go to Row
/// past the indexed rows, Copy as tab-separated text, and the cell
/// inspector.
@MainActor
final class FindTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-find-\(UUID().uuidString)")
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

    private func file(_ name: String, _ bytes: Data) throws -> URL {
        let url = directory.appending(path: name)
        try bytes.write(to: url)
        return url
    }

    private func file(_ name: String, _ text: String) throws -> URL {
        try file(name, Data(text.utf8))
    }

    private func open(_ url: URL) throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        // Copy never touches the user's clipboard in tests.
        content.pasteboard = NSPasteboard(name: NSPasteboard.Name("io.github.robhaswell.leal.tests.\(UUID().uuidString)"))
        addTeardownBlock { @MainActor in content.pasteboard.releaseGlobally() }
        return (document, try XCTUnwrap(document.model), content)
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

    private func searchSettled(_ content: DocumentViewController) async throws {
        try await waitUntil("the search finished") { !content.find.isSearching && content.find.pendingStep == nil }
    }

    /// A file of about `bytes` bytes: a header and numbered rows; every
    /// 7th row's customer is "Marlow Foods", and every 9th has a quoted
    /// note over two lines.
    private func bigFile(_ name: String, bytes: Int) throws -> (URL, rows: Int, marlow: [Int]) {
        var text = "id,customer,notes\n"
        var rows = 0
        var marlow: [Int] = []
        while text.utf8.count < bytes {
            var chunk = ""
            for _ in 0..<1_000 {
                let customer = rows % 7 == 3 ? "Marlow Foods" : "Ostrava \(rows)"
                if rows % 7 == 3 { marlow.append(rows) }
                let notes = rows % 9 == 0 ? "\"two\nlines\"" : "n\(rows)"
                chunk += "\(rows),\(customer),\(notes)\n"
                rows += 1
            }
            text += chunk
        }
        return (try file(name, text), rows, marlow)
    }

    private static let orders = [
        "order,customer,email,notes",
        "1,Marlow Foods,orders@marlow.example,",
        "2,Ostrava Tools,orders@ostrava.example,",
        "3,\"Marlow & Daughters\",x@y,\"say \"\"marlow\"\"\"",
        "4,Pinecrest,p@q,",
        "5,marlowe,MARLOW,last",
    ].joined(separator: "\n") + "\n"

    // MARK: Find

    func testFindCountsAndSelectsMatchesFromTheCore() async throws {
        let (document, model, content) = try open(try file("orders.csv", Self.orders))
        try await waitUntil("indexed") { model.isIndexComplete }
        content.showFind(nil)
        XCTAssertTrue(content.isFindBarShown)
        XCTAssertTrue(content.grid.highlighter === content.find)
        content.findBar.field.stringValue = "MARLOW"
        content.search(for: "MARLOW")
        try await searchSettled(content)
        // Grid rows (the header row isn't one): 0, 2, 4. Cells: (0,1),
        // (0,2), (2,1), (2,3), (4,1), (4,2).
        XCTAssertEqual(content.find.matchCount, 6)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 0, column: 1), "typing selects the first match")
        XCTAssertEqual(content.findBar.count.stringValue, "1 of 6")
        XCTAssertTrue(content.findBar.arrows.isEnabled)
        // Highlights: where the query is, from the core.
        let highlight = try XCTUnwrap(content.find.highlight(row: 0, column: 2))
        XCTAssertEqual(highlight.ranges, [NSRange(location: 7, length: 6)])
        XCTAssertFalse(highlight.isCurrent)
        XCTAssertEqual(content.find.highlight(row: 0, column: 1)?.isCurrent, true)
        XCTAssertNil(content.find.highlight(row: 1, column: 1))
        // A click on another match: it becomes the current one.
        content.grid.select(CellPosition(row: 2, column: 3))
        XCTAssertEqual(content.findBar.count.stringValue, "4 of 6")
        content.grid.select(CellPosition(row: 1, column: 0))
        XCTAssertEqual(content.findBar.count.stringValue, "6 matches")

        // Case matters when Ignore Case is off.
        content.find.find("marlow", caseSensitive: true, from: nil)
        try await searchSettled(content)
        XCTAssertEqual(content.find.matchCount, 3)
        document.close()
    }

    func testNextAndPreviousWrapAtTheEdges() async throws {
        let (document, model, content) = try open(try file("orders.csv", Self.orders))
        try await waitUntil("indexed") { model.isIndexComplete }
        content.showFind(nil)
        content.findBar.field.stringValue = "marlow"
        content.search(for: "marlow")
        try await searchSettled(content)
        let active = { content.grid.activeCell }
        XCTAssertEqual(active(), CellPosition(row: 0, column: 1))
        var visited = [active()]
        for _ in 0..<5 {
            content.findNext(nil)
            try await searchSettled(content)
            visited.append(active())
        }
        XCTAssertEqual(visited, [
            CellPosition(row: 0, column: 1), CellPosition(row: 0, column: 2), CellPosition(row: 2, column: 1),
            CellPosition(row: 2, column: 3), CellPosition(row: 4, column: 1), CellPosition(row: 4, column: 2),
        ])
        XCTAssertEqual(content.findBar.count.stringValue, "6 of 6")
        // VoiceOver hears each step's count.
        XCTAssertEqual(content.lastAnnouncement, "6 of 6")
        XCTAssertEqual(content.wrapIndicator.shown, 0)
        // Past the last: round to the first, with the "wrapped" sign and
        // no beep; before the first: the last.
        let missesBeforeWrap = content.find.misses
        content.findNext(nil)
        try await searchSettled(content)
        XCTAssertEqual(active(), CellPosition(row: 0, column: 1))
        XCTAssertEqual(content.findBar.count.stringValue, "1 of 6")
        XCTAssertEqual(content.wrapIndicator.shown, 1)
        XCTAssertFalse(content.wrapIndicator.isHidden)
        XCTAssertEqual(content.lastAnnouncement, "Wrapped to the first match. 1 of 6")
        XCTAssertEqual(content.find.misses, missesBeforeWrap, "no beep")
        content.findPrevious(nil)
        try await searchSettled(content)
        XCTAssertEqual(active(), CellPosition(row: 4, column: 2))
        XCTAssertEqual(content.wrapIndicator.shown, 2)
        XCTAssertEqual(content.lastAnnouncement, "Wrapped to the last match. 6 of 6")
        content.findPrevious(nil)
        try await searchSettled(content)
        XCTAssertEqual(active(), CellPosition(row: 4, column: 1))
        XCTAssertEqual(content.wrapIndicator.shown, 2)
        XCTAssertEqual(content.lastAnnouncement, "5 of 6")

        // Nothing to find: the cell stays, and the count says so.
        content.findBar.field.stringValue = "zebra"
        content.search(for: "zebra")
        try await searchSettled(content)
        XCTAssertEqual(content.findBar.count.stringValue, "No matches")
        XCTAssertFalse(content.findBar.arrows.isEnabled)
        let misses = content.find.misses
        content.findNext(nil)
        try await searchSettled(content)
        XCTAssertEqual(content.find.misses, misses + 1)
        XCTAssertEqual(active(), CellPosition(row: 4, column: 1))

        // Done: the search and highlights go; ⌘G brings them back.
        content.findBar.field.stringValue = "marlow"
        content.hideFindBar()
        XCTAssertFalse(content.isFindBarShown)
        XCTAssertNil(content.grid.highlighter)
        XCTAssertNil(content.find.search)
        content.findNext(nil)
        XCTAssertTrue(content.isFindBarShown)
        try await searchSettled(content)
        XCTAssertEqual(active(), CellPosition(row: 4, column: 2))
        document.close()
    }

    /// The search runs while the file is still being indexed, and keeps up
    /// with it; Go to Row past the indexed rows shows the row once the
    /// index reaches it (DESIGN §3.10 rule 5).
    func testFindAndGoToRowWorkWhileIndexing() async throws {
        let (url, rows, marlow) = try bigFile("big.csv", bytes: 24 << 20)
        let (document, model, content) = try open(url)
        // Nothing has run on the main actor since opening: the model still
        // has only the first 64 KB's rows.
        XCTAssertFalse(model.isIndexComplete)
        let loaded = model.loadedRowCount
        XCTAssertLessThan(loaded, rows / 10)

        // Search, from the top: typing would select the first match.
        content.showFind(nil)
        content.findBar.field.stringValue = "marlow foods"
        content.find.find("marlow foods", caseSensitive: false, from: nil)
        XCTAssertTrue(content.find.isSearching)
        XCTAssertNotNil(content.find.pendingStep)

        // Go to a row far past the indexed ones. The user went somewhere,
        // so the search no longer moves the selection.
        let target = rows - 10
        content.goTo(rowNumber: target + 1)
        XCTAssertNil(content.find.pendingStep)
        XCTAssertEqual(content.grid.pendingJump, .row(target))
        XCTAssertNil(content.grid.activeCell)
        XCTAssertTrue(content.grid.visibleRows.contains(min(target, model.rowCount - 1)))
        XCTAssertFalse(content.grid.pill.isHidden)

        try await waitUntil("the row was reached") { content.grid.activeCell?.row == target }
        XCTAssertNil(content.grid.pendingJump)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: target, column: 0), "in the column it was asked from")
        XCTAssertTrue(content.grid.visibleRows.contains(target))
        try await searchSettled(content)
        XCTAssertEqual(content.find.matchCount, UInt64(marlow.count))
        XCTAssertEqual(content.findBar.count.stringValue, "\(marlow.count.formatted()) matches")
        // Next from there: the next match, or round to the first.
        content.findNext(nil)
        try await searchSettled(content)
        let next = marlow.first { $0 >= target } ?? marlow[0]
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: next, column: 1))
        // And Previous from the first match wraps round to the last.
        content.grid.select(CellPosition(row: marlow[0], column: 1))
        content.findPrevious(nil)
        try await searchSettled(content)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: marlow.last!, column: 1))
        XCTAssertEqual(content.find.current?.ordinal, UInt64(marlow.count))
        XCTAssertEqual(content.findBar.count.stringValue, "\(marlow.count.formatted()) of \(marlow.count.formatted())")
        document.close()
    }

    func testReadingTheFileAgainSearchesAgain() async throws {
        let (document, model, content) = try open(try file("semi.csv", "a;b\nx;marlow\nmarlow;y\n"))
        try await waitUntil("indexed") { model.isIndexComplete }
        content.showFind(nil)
        content.findBar.field.stringValue = "marlow"
        content.search(for: "marlow")
        try await searchSettled(content)
        XCTAssertEqual(content.find.matchCount, 2)
        // The grid row of "x;marlow", whether or not "a;b" is taken as a
        // header row.
        let first = 1 - model.headerRows
        XCTAssertEqual(content.find.highlight(row: first, column: 1)?.ranges, [NSRange(location: 0, length: 6)])
        // As commas, each row is one field, so the matches are elsewhere:
        // the search starts again for the new reading.
        content.treatAs(.comma)
        try await searchSettled(content)
        XCTAssertEqual(content.find.matchCount, 2)
        let progress = try XCTUnwrap(content.find.progress)
        XCTAssertEqual(progress.generation, model.generation)
        XCTAssertGreaterThan(progress.generation, 0)
        XCTAssertNil(content.find.highlight(row: 1 - model.headerRows, column: 1))
        document.close()
    }

    // MARK: Copy

    func testCopyPutsTabSeparatedDisplayValuesOnThePasteboard() async throws {
        let text = "a,b,c\nplain,\"tab\there\",\"two\nlines\"\n\"say \"\"hi\"\"\",caf\u{e9},end\nshort\n"
        let (document, model, content) = try open(try file("copy.csv", text))
        try await waitUntil("indexed") { model.isIndexComplete }
        // The grid row of "plain", whether or not "a,b,c" is taken as a
        // header row.
        let first = 1 - model.headerRows
        content.grid.select(CellPosition(row: first, column: 0))
        content.grid.extend(to: CellPosition(row: first + 2, column: 2))
        content.copySelection()
        await content.copyTask?.value
        let expected = "plain\t\"tab\there\"\t\"two\nlines\"\n\"say \"\"hi\"\"\"\tcaf\u{e9}\tend\nshort\t\t"
        XCTAssertEqual(content.pasteboard.string(forType: .string), expected)
        XCTAssertEqual(content.pasteboard.string(forType: .tabularText), expected)
        // One cell: just its value.
        content.grid.select(CellPosition(row: first + 1, column: 1))
        content.copySelection()
        await content.copyTask?.value
        XCTAssertEqual(content.pasteboard.string(forType: .string), "caf\u{e9}")
        document.close()
    }

    func testSelectAllCopiesEveryRowEvenWhileIndexing() async throws {
        let (url, rows, _) = try bigFile("all.csv", bytes: 2 << 20)
        let (document, model, content) = try open(url)
        XCTAssertFalse(model.isIndexComplete)
        content.grid.gridView.selectAll(nil)
        XCTAssertTrue(content.grid.selection?.throughLastRow ?? false)
        content.copySelection()
        await content.copyTask?.value
        let copied = try XCTUnwrap(content.pasteboard.string(forType: .string))
        // Every data row, one line each, plus the second line of every 9th
        // row's two-line note.
        let lines = copied.split(separator: "\n", omittingEmptySubsequences: false)
        XCTAssertEqual(lines.count, rows + (rows + 8) / 9)
        XCTAssertEqual(lines.first, "0\tOstrava 0\t\"two")
        let last = rows - 1
        let lastLine = last % 9 == 0
            ? "lines\""
            : "\(last)\t\(last % 7 == 3 ? "Marlow Foods" : "Ostrava \(last)")\tn\(last)"
        XCTAssertEqual(lines.last.map(String.init), lastLine)
        document.close()
    }

    /// Copy doesn't wait for the scheduler's 250 ms of idle input (the 1.8
    /// review's must-fix): a small copy is on the pasteboard the moment
    /// ⌘C returns, straight after a key or a scroll.
    func testASmallCopyIsOnThePasteboardAtOnce() async throws {
        let (document, model, content) = try open(try file("orders.csv", Self.orders))
        try await waitUntil("indexed") { model.isIndexComplete }
        content.grid.select(CellPosition(row: 0, column: 1))
        content.grid.extend(to: CellPosition(row: 1, column: 2))
        environment.scheduler.noteUserInput()
        content.copySelection()
        XCTAssertNil(content.copyTask, "no job: copied on the spot")
        XCTAssertEqual(
            content.pasteboard.string(forType: .string),
            "Marlow Foods\torders@marlow.example\nOstrava Tools\torders@ostrava.example"
        )
        document.close()
    }

    /// A copy that has to wait for the index promises its text to the
    /// pasteboard at once, so a paste never gets the old clipboard; a
    /// paste meanwhile waits for it.
    func testALargeCopyIsPromisedAtOnceAndAsksFirstWhenHuge() async throws {
        // Over `immediateCopyBytes`, so it is a job whatever the index has.
        let (url, rows, _) = try bigFile("promise.csv", bytes: 6 << 20)
        let (document, model, content) = try open(url)
        XCTAssertFalse(model.isIndexComplete)
        content.pasteboard.clearContents()
        content.pasteboard.setString("old", forType: .string)
        // Hold P2 work, so the copy can't finish yet.
        environment.scheduler.setInteracting(interacting: true)
        content.grid.gridView.selectAll(nil)
        content.copySelection()
        XCTAssertNotNil(content.copyPromise)
        // (macOS adds the types' legacy names.)
        XCTAssertTrue(Set(content.pasteboard.types ?? []).isSuperset(of: Clipboard.types), "the promise is there at once")
        XCTAssertEqual(content.pasteboard.pasteboardItems?.count, 1)
        environment.scheduler.setInteracting(interacting: false)
        let copied = try XCTUnwrap(content.pasteboard.string(forType: .string), "the paste waits for the copy")
        XCTAssertEqual(copied.split(separator: "\n", omittingEmptySubsequences: false).count, rows + (rows + 8) / 9)
        XCTAssertEqual(content.pasteboard.string(forType: .tabularText), copied)
        XCTAssertFalse(try XCTUnwrap(content.copyPromise).isHoldingText)

        // Over the limit, it asks first, and Cancel copies nothing.
        content.askBeforeCopyBytes = 1 << 20
        var asked: [UInt64] = []
        content.confirmLargeCopy = { bytes, answer in
            asked.append(bytes)
            answer(false)
        }
        content.pasteboard.clearContents()
        content.copySelection()
        XCTAssertEqual(asked.count, 1)
        XCTAssertGreaterThan(asked[0], 1 << 20)
        XCTAssertNil(content.copyPromise)
        XCTAssertNil(content.pasteboard.string(forType: .string))
        document.close()
    }

    /// Closing the window stops its search at once, so it doesn't keep the
    /// core's document, and so its file, open (1.8 review).
    func testClosingTheWindowStopsItsSearch() async throws {
        let (url, _, _) = try bigFile("close.csv", bytes: 6 << 20)
        let (document, _, content) = try open(url)
        // Hold P2 work, so the search is still running when the window
        // closes.
        environment.scheduler.setInteracting(interacting: true)
        defer { environment.scheduler.setInteracting(interacting: false) }
        content.showFindBar()
        content.find.find("marlow", caseSensitive: false, from: nil)
        let search = try XCTUnwrap(content.find.search?.job())
        XCTAssertFalse(search.isFinished())
        document.close()
        XCTAssertNil(content.find.search)
        try await waitUntil("the search stopped", timeout: 10) { search.isFinished() }
    }

    /// The tab-separated text of every data row of `bigFile(rows:)`'s file.
    private func expectedCopy(rows: Int) -> String {
        (0..<rows).map { row in
            let customer = row % 7 == 3 ? "Marlow Foods" : "Ostrava \(row)"
            let notes = row % 9 == 0 ? "\"two\nlines\"" : "n\(row)"
            return "\(row)\t\(customer)\t\(notes)"
        }.joined(separator: "\n")
    }

    /// ⌘A ⌘C on a file big enough that the copy is promised, with P2 work
    /// held so the copy is still running when `then` happens; then a paste.
    /// The paste gets exactly the copied cells, and afterwards the promise
    /// lets go of the text and the core's job (1.8 re-review).
    private func copyThenPaste(
        _ name: String,
        then: @MainActor (CSVDocument, DocumentViewController) async throws -> Void
    ) async throws {
        let (url, rows, _) = try bigFile(name, bytes: 6 << 20)
        let (document, model, content) = try open(url)
        try await waitUntil("indexed") { model.isIndexComplete }
        let pasteboard = content.pasteboard
        environment.scheduler.setInteracting(interacting: true)
        content.grid.gridView.selectAll(nil)
        content.copySelection()
        let promise = try XCTUnwrap(content.copyPromise)
        let job = try XCTUnwrap(promise.job?.job())
        XCTAssertFalse(job.isFinished(), "still running")
        try await then(document, content)
        XCTAssertFalse(job.isFinished(), "not stopped by it")
        environment.scheduler.setInteracting(interacting: false)
        let pasted = pasteboard.string(forType: .string)
        XCTAssertEqual(pasted, expectedCopy(rows: rows))
        XCTAssertEqual(pasteboard.string(forType: .tabularText), pasted)
        XCTAssertFalse(promise.isHoldingText, "the text goes once both types are given")
        for document in NSDocumentController.shared.documents { document.close() }
    }

    func testACopyClosedBeforeItsPasteStillPastesItsCells() async throws {
        try await copyThenPaste("closed.csv") { document, _ in
            document.close()
        }
    }

    func testACopyStillPastesItsCellsAfterTheFileIsReadAgain() async throws {
        try await copyThenPaste("reread.csv") { _, content in
            content.treatAs(.semicolon)
            XCTAssertEqual(content.model.interpretation.delimiter, .semicolon)
        }
    }

    /// **Reload** (task 1.9) releases the core document and opens a new one
    /// of the file as it is now. A copy promised before it still pastes
    /// exactly the cells it was made from, though the file has changed.
    func testACopyStillPastesItsCellsAfterTheFileIsReloaded() async throws {
        try await copyThenPaste("reloaded.csv") { document, content in
            let url = try XCTUnwrap(document.fileURL)
            try Data("id,customer,notes\n0,Changed Elsewhere,x\n".utf8).write(to: url, options: .atomic)
            content.reloadFromDisk(nil)
            XCTAssertEqual(content.model.cell(row: 0, column: 1), .text("Changed Elsewhere", truncated: false))
        }
    }

    // MARK: Reload with the find bar and the inspector open (task 1.9)

    /// An open find runs again on the reloaded file: its matches are the new
    /// file's, not the old one's.
    func testReloadRunsAnOpenFindAgain() async throws {
        let url = try file("orders.csv", Self.orders)
        let (document, model, content) = try open(url)
        try await waitUntil("indexed") { model.isIndexComplete }
        content.showFind(nil)
        content.findBar.field.stringValue = "MARLOW"
        content.search(for: "MARLOW")
        try await searchSettled(content)
        XCTAssertEqual(content.find.matchCount, 6)

        let fewer = "order,customer,email,notes\n1,Marlow Foods,orders@example,\n2,Ostrava Tools,o@example,\n"
        try Data(fewer.utf8).write(to: url, options: .atomic)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        content.reloadFromDisk(nil)
        try await waitUntil("indexed again") { model.isIndexComplete }
        try await searchSettled(content)
        XCTAssertTrue(content.isFindBarShown)
        XCTAssertEqual(content.find.query, "MARLOW")
        XCTAssertEqual(content.find.matchCount, 1, "the new file's matches")
        XCTAssertNotNil(content.find.highlight(row: 0, column: 1))
        XCTAssertNil(content.find.highlight(row: 2, column: 1), "no match from the old file")
        document.close()
    }

    /// The inspector shows the restored cell's value as it is after Reload.
    func testReloadRefreshesTheInspector() async throws {
        let url = try file("notes.csv", "id,notes\n1,old note\n2,second\n")
        let (document, model, content) = try open(url)
        try await waitUntil("indexed") { model.isIndexComplete }
        content.grid.select(CellPosition(row: 0, column: 1))
        content.toggleCellInspector(nil)
        await content.inspectorTask?.value
        XCTAssertEqual(content.inspector.textView.string, "old note")

        try Data("id,notes\n1,new note\n2,second\n".utf8).write(to: url, options: .atomic)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        content.reloadFromDisk(nil)
        XCTAssertTrue(content.isInspectorShown)
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 0, column: 1))
        await content.inspectorTask?.value
        try await waitUntil("the new value is shown") { content.inspector.textView.string == "new note" }
        document.close()
    }

    /// A new copy replaces a promise still running: the pasteboard lets go
    /// of it, which stops its job.
    func testANewCopyStopsTheCopyItReplaces() async throws {
        let (url, _, _) = try bigFile("replace.csv", bytes: 6 << 20)
        let (document, model, content) = try open(url)
        try await waitUntil("indexed") { model.isIndexComplete }
        environment.scheduler.setInteracting(interacting: true)
        defer { environment.scheduler.setInteracting(interacting: false) }
        content.grid.gridView.selectAll(nil)
        content.copySelection()
        let job = try XCTUnwrap(content.copyPromise?.job?.job())
        content.grid.select(CellPosition(row: 0, column: 0))
        content.copySelection()
        XCTAssertEqual(content.pasteboard.string(forType: .string), "0")
        try await waitUntil("the replaced copy stopped", timeout: 10) { job.isFinished() }
        document.close()
    }

    // MARK: The cell inspector (mockup 05a)

    func testTheInspectorShowsWholeValues() async throws {
        let long = String(repeating: "x", count: 1_200_000)
        // Enough é (UTF-8) that the invalid byte doesn't make it Windows-1252.
        var bytes = Data("id,notes,code\n1,\"Deliver to the rear entrance.\nCall on arrival.\nGate code 4471\",caf\u{e9} cr\u{e8}me br\u{fb}l\u{e9}e\n".utf8)
        bytes.append(Data("2,bad ".utf8) + Data([0xFF]) + Data(" byte,\(long)\n3\n".utf8))
        let (document, model, content) = try open(try file("inspect.csv", bytes))
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertFalse(content.isInspectorShown)
        content.grid.select(CellPosition(row: 0, column: 1))
        content.toggleCellInspector(nil)
        XCTAssertTrue(content.isInspectorShown)
        await content.inspectorTask?.value
        XCTAssertEqual(content.inspector.textView.string, "Deliver to the rear entrance.\nCall on arrival.\nGate code 4471")
        XCTAssertEqual(content.inspector.columnLabel.stringValue, "notes")
        XCTAssertEqual(content.inspector.rowLabel.stringValue, "Row 1")
        XCTAssertEqual(content.inspector.sizeLabel.stringValue, "3 lines · 61 characters")

        // An invalid byte: the U+FFFD the grid shows, and a note.
        content.grid.select(CellPosition(row: 1, column: 1))
        await content.inspectorTask?.value
        XCTAssertEqual(content.inspector.textView.string, "bad \u{FFFD} byte")
        XCTAssertEqual(content.inspector.sizeLabel.stringValue, "1 line · 10 characters · invalid bytes shown as �")

        // A very long value with no line breaks: its first 64,000
        // characters, said so, and shown without stalling the window (a
        // million characters took 800 ms on an M5 Pro; the 1.8 review).
        content.grid.select(CellPosition(row: 1, column: 2))
        let started = Date()
        await content.inspectorTask?.value
        content.view.layoutSubtreeIfNeeded()
        content.view.displayIfNeeded()
        let elapsed = Date().timeIntervalSince(started)
        XCTAssertEqual(content.inspector.textView.string.count, Int(DocumentModel.inspectorMaxCharacters))
        XCTAssertEqual(content.inspector.sizeLabel.stringValue, "1 line · 1,200,000 characters · value truncated: the first 64,000 shown")
        XCTAssertLessThan(elapsed, 0.25, "showing a long value took \(elapsed) s")

        // A short row's missing cell.
        content.grid.select(CellPosition(row: 2, column: 2))
        await content.inspectorTask?.value
        XCTAssertEqual(content.inspectorContent, .note(InspectorText.missing))
        XCTAssertEqual(content.inspector.sizeLabel.stringValue, "")

        // ⌘I again hides it.
        content.toggleCellInspector(nil)
        XCTAssertFalse(content.isInspectorShown)
        let item = NSMenuItem(title: "", action: #selector(DocumentViewController.toggleCellInspector(_:)), keyEquivalent: "i")
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.title, "Show Cell Inspector")
        content.setInspectorShown(true)
        XCTAssertTrue(content.validateMenuItem(item))
        XCTAssertEqual(item.title, "Hide Cell Inspector")
        document.close()
    }

    /// The find bar and the inspector clip their fills to themselves: since
    /// the macOS 14 SDK a view's `draw(_:)` rectangle can reach past it,
    /// and the inspector's once covered the whole grid (1.6's "Clipping").
    func testTheFindBarAndInspectorDontCoverTheGrid() async throws {
        let (document, model, content) = try open(try file("orders.csv", Self.orders))
        try await waitUntil("indexed") { model.isIndexComplete }
        content.view.window?.appearance = NSAppearance(named: .aqua)
        content.showFindBar()
        content.setInspectorShown(true)
        await content.inspectorTask?.value
        let root = content.view
        root.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(root.bitmapImageRepForCachingDisplay(in: root.bounds))
        root.cacheDisplay(in: root.bounds, to: rep)
        // The grid's first row (in the root view's unflipped coordinates)
        // has text.
        let grid = content.grid.convert(content.grid.bounds, to: root)
        let firstRow = NSRect(x: grid.minX + 80, y: rep.size.height - grid.maxY + GridMetrics.headerHeight, width: 300, height: GridMetrics.rowHeight)
        let scale = CGFloat(rep.pixelsWide) / rep.size.width
        var dark = 0
        for y in Int(firstRow.minY * scale)..<Int(firstRow.maxY * scale) {
            for x in Int(firstRow.minX * scale)..<Int(firstRow.maxX * scale) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.brightnessComponent < 0.5 { dark += 1 }
            }
        }
        XCTAssertGreaterThan(dark, 50, "the grid's first row is drawn")
        document.close()
    }

    func testTheInspectorWaitsForARowPastTheIndex() async throws {
        let (url, rows, _) = try bigFile("wait.csv", bytes: 24 << 20)
        let (document, model, content) = try open(url)
        XCTAssertFalse(model.isIndexComplete)
        content.setInspectorShown(true)
        content.goTo(rowNumber: rows - 5)
        // The row isn't read yet; the inspector says so, then shows it.
        try await waitUntil("the row was reached") { content.grid.activeCell?.row == rows - 6 }
        await content.inspectorTask?.value
        try await waitUntil("the value was shown") { content.inspector.textView.string == "\(rows - 6)" }
        XCTAssertEqual(content.inspector.rowLabel.stringValue, "Row \((rows - 5).formatted())")
        XCTAssertEqual(content.inspector.columnLabel.stringValue, "id")
        document.close()
    }
}
