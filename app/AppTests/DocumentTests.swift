import AppKit
import LealFFI
import XCTest

@testable import Leal

/// `CSVDocument` and `DocumentModel` against the real core, hosted in
/// Leal.app: opening and closing, the first screen before the index, rows
/// arriving while indexing, failure after a panic (DESIGN §3.9),
/// cancellation (ADR-0005 decision 6), the Header row toggle and UTF-16.
@MainActor
final class DocumentTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-app-\(UUID().uuidString)")
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

    private func file(_ name: String, _ contents: String) throws -> URL {
        let url = directory.appending(path: name)
        try Data(contents.utf8).write(to: url)
        return url
    }

    private func open(_ url: URL) throws -> CSVDocument {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        return document
    }

    /// Waits (letting the main actor run) until `condition` holds.
    private func waitUntil(_ what: String, timeout: TimeInterval = 20, _ condition: () -> Bool) async throws {
        let deadline = Date().addingTimeInterval(timeout)
        while !condition() {
            if Date() > deadline {
                XCTFail("timed out waiting until \(what)")
                return
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// A file of about `bytes` bytes: a header and numbered rows, some with
    /// a quoted field holding a newline.
    private func bigFile(_ name: String, bytes: Int) throws -> (URL, rows: Int) {
        var text = "id,name,amount,notes\n"
        var rows = 0
        while text.utf8.count < bytes {
            var chunk = ""
            for _ in 0..<1_000 {
                let notes = rows % 9 == 0 ? "\"two\nlines\"" : "n\(rows)"
                chunk += "\(rows),name \(rows),\(rows % 997).25,\(notes)\n"
                rows += 1
            }
            text += chunk
        }
        return (try file(name, text), rows)
    }

    private func records() -> [String] {
        (try? FileManager.default.contentsOfDirectory(atPath: environment.temp.recordsDir)) ?? []
    }

    // MARK: Open and close

    func testOpeningShowsTheFileAndClosingReleasesIt() async throws {
        let url = try file("orders.csv", "order_id,qty,total,notes\r\nA-1,40,1196.00,Gift wrap\r\nA-2,3,12.30,\r\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        XCTAssertEqual(document.windowControllers.count, 1)
        XCTAssertEqual(model.headerTitle(column: 0), HeaderTitle(text: "order_id", style: .name))
        XCTAssertEqual(model.columnCount, 4)
        XCTAssertEqual(model.cell(row: 0, column: 0), .text("A-1", truncated: false))
        XCTAssertEqual(model.cell(row: 1, column: 3), .text("", truncated: false))
        XCTAssertTrue(model.isNumeric(column: 1))
        XCTAssertTrue(model.isNumeric(column: 2))
        XCTAssertFalse(model.isNumeric(column: 0))
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 2)
        XCTAssertEqual(model.loadedRowCount, 2)
        XCTAssertEqual(
            StatusText.segments(model.status),
            ["2 rows × 4 columns", "Comma", "CRLF", "UTF-8"]
        )
        // The clone is recorded while the file is open, and gone once it is
        // closed (DESIGN §3.1).
        XCTAssertFalse(records().isEmpty)
        let calls = model.coreCalls
        document.close()
        try await waitUntil("the clone is removed") { records().isEmpty }
        _ = model.cell(row: 0, column: 0)
        XCTAssertEqual(model.coreCalls, calls, "no calls after closing")
    }

    /// Task 2.0a review, item 4: a grid's read ahead queued when its
    /// document closes doesn't keep the core's document (and the file's
    /// clone) alive until it runs, and reads nothing when it does.
    func testAReadAheadQueuedWhenTheDocumentClosesDoesntKeepIt() async throws {
        let (url, _) = try bigFile("closing.csv", bytes: 200_000)
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertFalse(records().isEmpty)
        let back = model.readsAheadBack
        CellTileCache.suspendReadsAhead()
        model.readAhead(rows: 640..<704, columns: 0..<4)
        let calls = model.coreCalls
        document.close()
        try await waitUntil("the clone is removed, with the read still queued") { records().isEmpty }
        CellTileCache.resumeReadsAhead()
        try await waitUntil("the read is back") { model.readsAheadBack > back }
        XCTAssertEqual(model.coreCalls, calls, "no calls after closing")
        XCTAssertNil(model.cachedCell(row: 640, column: 0))
    }

    func testAFileThatCantBeOpenedIsWorded() throws {
        let url = directory.appending(path: "missing.csv")
        XCTAssertThrowsError(try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")) { error in
            let error = error as NSError
            XCTAssertEqual(error.localizedDescription, "Leal couldn’t open “missing.csv”.")
            XCTAssertEqual(error.localizedRecoverySuggestion, "The file doesn’t exist.")
        }
    }

    // MARK: First paint and indexing (DESIGN §3.10)

    func testTheFirstScreenIsShownBeforeTheIndexAndRowsArriveAsItGrows() async throws {
        let (url, rows) = try bigFile("big.csv", bytes: 24 << 20)
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        // Nothing has run on the main actor since the open, so this is what
        // first paint gave: the rows in the first 64 KB and an estimate.
        let firstLoaded = model.loadedRowCount
        XCTAssertGreaterThan(firstLoaded, 100)
        XCTAssertLessThan(firstLoaded, rows / 10)
        XCTAssertFalse(model.isIndexComplete)
        XCTAssertEqual(Double(model.rowCount), Double(rows), accuracy: Double(rows) * 0.35, "the scrollbar's estimate (this file's rows get longer as their numbers grow)")
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let grid = controller.content.grid
        XCTAssertEqual(grid.gridView.frame.height, CGFloat(model.rowCount) * GridMetrics.rowHeight)
        // The grid draws the first rows now, from the first screen.
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))
        XCTAssertEqual(model.cell(row: firstLoaded + 10, column: 1), .notLoaded)
        XCTAssertTrue(StatusText.counts(model.status).hasPrefix("Indexing… about "))

        // Rows arrive as the index grows, and the count becomes exact.
        var seen = [firstLoaded]
        try await waitUntil("indexed") {
            if model.loadedRowCount != seen.last { seen.append(model.loadedRowCount) }
            return model.isIndexComplete
        }
        XCTAssertEqual(seen, seen.sorted(), "the loaded rows only grow")
        XCTAssertEqual(model.loadedRowCount, rows)
        XCTAssertEqual(model.rowCount, rows)
        XCTAssertEqual(grid.gridView.frame.height, CGFloat(rows) * GridMetrics.rowHeight)
        XCTAssertEqual(model.cell(row: rows - 1, column: 0), .text("\(rows - 1)", truncated: false))
        XCTAssertEqual(model.cell(row: 9, column: 3), .text("two\nlines", truncated: false))
        XCTAssertEqual(StatusText.counts(model.status), "\(rows.formatted()) rows × 4 columns")
        document.close()
    }

    // MARK: Failure (DESIGN §3.9)

    func testAPanicFailsTheDocumentWhichMakesNoMoreCallsAndOffersToReopen() async throws {
        let url = try file("a.csv", "a,b\n1,2\n3,4\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        XCTAssertFalse(model.isFailed)
        _ = model.call { try $0.debugPanic() }
        XCTAssertTrue(model.isFailed)
        XCTAssertTrue(document.isOfferingReopen)
        XCTAssertEqual(OpenErrorText.describe(try XCTUnwrap(model.failure)), "Something went wrong inside Leal. Close the file and open it again.")
        let calls = model.coreCalls
        XCTAssertEqual(model.rowCount, 0)
        XCTAssertEqual(model.loadedRowCount, 0)
        _ = model.cell(row: 0, column: 0)
        model.prepare(rows: 0..<10, columns: 0..<2)
        model.setHeaderRow(false)
        XCTAssertNil(model.call { try $0.rowCount() })
        XCTAssertEqual(model.coreCalls, calls, "no calls on a failed document's handle")

        // Reopening makes a new, working document for the same file.
        let reopened = await withCheckedContinuation { continuation in
            document.reopenAfterFailure(display: false) { continuation.resume(returning: $0) }
        }
        let again = try XCTUnwrap((reopened as? CSVDocument)?.model)
        XCTAssertFalse(again.isFailed)
        XCTAssertEqual(again.cell(row: 0, column: 1), .text("2", truncated: false))
        XCTAssertEqual(reopened?.fileURL, url)
    }

    func testAPanicInABackgroundJobFailsTheDocument() async throws {
        let url = try file("a.csv", "a,b\n1,2\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        let job = debugPanickingJob(scheduler: environment.scheduler)
        _ = model.call { $0.debugWatch(job: job) }
        do {
            try await job.finish()
            XCTFail("the job should panic")
        } catch let failure as JobFailure {
            XCTAssertEqual(failure, .Panicked(message: "deliberate job panic"))
        }
        // The core marks the document as the job ends, on the job's thread.
        try await waitUntil("the core marks the document failed") {
            _ = model.call { try $0.rowCount() }
            return model.isFailed
        }
        XCTAssertTrue(document.isOfferingReopen)
    }

    // MARK: Cancellation (ADR-0005 decision 6)

    /// Cancelling the Swift task that awaits a job stops the Rust job.
    func testCancellingTheWaitingTaskStopsTheJob() async throws {
        let (url, _) = try bigFile("big.csv", bytes: 4 << 20)
        // Hold background work, as while the user scrolls, so the review
        // is still waiting when its task is cancelled.
        environment.scheduler.setInteracting(interacting: true)
        defer { environment.scheduler.setInteracting(interacting: false) }
        let document = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: environment.temp,
            scheduler: environment.scheduler,
            options: OpenOptions(),
            observer: nil
        )
        let review = try document.reviewJob()
        let waiting = Task { try await review.finish() }
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(review.isFinished(), "held while interacting")
        waiting.cancel()
        let result = await waiting.result
        XCTAssertThrowsError(try result.get()) { error in
            XCTAssertEqual(error as? JobFailure, .Cancelled)
        }
        XCTAssertTrue(review.isFinished())

        // A task cancelled before it waits cancels the job at once. Another
        // document's review is held too (the scheduler is still
        // interacting), so the job is certainly still running.
        let other = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: environment.temp,
            scheduler: environment.scheduler,
            options: OpenOptions(),
            observer: nil
        )
        let held = try other.reviewJob()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(held.isFinished(), "held while interacting")
        let early = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            try await held.finish()
        }
        let earlyResult = await early.result
        XCTAssertThrowsError(try earlyResult.get()) { error in
            XCTAssertEqual(error as? JobFailure, .Cancelled)
        }
        XCTAssertTrue(held.isFinished())
    }

    /// A window closed mid-gesture: closing the document ends its gesture,
    /// so background work of every document resumes (the scheduler has no
    /// timeout).
    func testClosingADocumentMidGestureLetsBackgroundWorkResume() async throws {
        let small = try file("a.csv", "a,b\n1,2\n")
        let document = try open(small)
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        controller.content.grid.scrollView.setGesture(true)

        // Another document's review is held by the gesture.
        let (url, _) = try bigFile("big.csv", bytes: 4 << 20)
        let other = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: environment.temp,
            scheduler: environment.scheduler,
            options: OpenOptions(),
            observer: nil
        )
        let review = try other.reviewJob()
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(review.isFinished(), "held during the gesture")

        document.close()
        try await waitUntil("the other document's review runs") { review.isFinished() }
    }

    // MARK: Failing while opening (DESIGN §3.9)

    /// A panic while the document opens fails it before there is a window
    /// to show the failure in: the open throws, worded, and nothing is left
    /// open.
    func testAPanicWhileOpeningIsAnOpenError() async throws {
        let url = try file("a.csv", "a,b\n1,2\n")
        DocumentModel.afterFirstPaintForTesting = { try $0.debugPanic() }
        defer { DocumentModel.afterFirstPaintForTesting = nil }
        XCTAssertThrowsError(try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")) { error in
            let error = error as NSError
            XCTAssertEqual(error.localizedDescription, "Leal couldn’t open “a.csv”.")
            XCTAssertEqual(error.localizedRecoverySuggestion, "Something went wrong inside Leal. Close the file and open it again.")
        }
        try await waitUntil("the clone is removed") { records().isEmpty }
    }

    /// A failure after opening but before the window is on screen is shown
    /// once the window is (not lost).
    func testAFailureBeforeTheWindowIsShownIsOfferedLater() throws {
        let url = try file("a.csv", "a,b\n1,2\n")
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        let model = try XCTUnwrap(document.model)
        _ = model.call { try $0.debugPanic() }
        XCTAssertTrue(model.isFailed)
        document.makeWindowControllers()
        XCTAssertTrue(document.isOfferingReopen)
        document.close()
    }

    /// A background job's panic reaches the model as a `JobFailure`, held
    /// as `any Error`. The alert still says what happened in the catalog's
    /// words, never UniFFI's debug text with the panic's message (phase 1
    /// review, app-1). The window keeps no stale rows or progress.
    func testTheAlertAfterAJobPanicIsWorded() async throws {
        let url = try file("a.csv", "a,b\n1,2\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        var sheets: [NSAlert] = []
        document.isOnScreen = { _ in true }
        document.showSheet = { alert, _, _ in sheets.append(alert) }
        let job = debugPanickingJob(scheduler: environment.scheduler)
        let ended: any Error
        do {
            try await job.finish()
            return XCTFail("the job should panic")
        } catch {
            ended = error
        }
        XCTAssertEqual(ended as? JobFailure, .Panicked(message: "deliberate job panic"))
        model.jobEnded(ended, job: .review, reading: model.readingID)

        XCTAssertTrue(model.isFailed)
        let alert = try XCTUnwrap(sheets.first)
        XCTAssertEqual(sheets.count, 1)
        XCTAssertEqual(alert.informativeText, "Something went wrong inside Leal. Close the file and open it again.")
        XCTAssertTrue(alert.messageText.hasPrefix("Leal can’t show “"))
        XCTAssertEqual(alert.buttons.map(\.title), ["Reopen", "Close"])
        XCTAssertEqual(model.status.rows, 0)
        XCTAssertEqual(model.status.columns, 0)
        XCTAssertFalse(model.status.indexing, "no progress bar for good")
        document.close()
    }

    /// A document that fails while its window is out of sight (minimised)
    /// offers to reopen once the window is back, once (phase 1 review,
    /// app-2).
    func testAFailureWhileTheWindowIsOutOfSightIsShownWhenItIsBack() throws {
        let document = try open(try file("a.csv", "a,b\n1,2\n"))
        let model = try XCTUnwrap(document.model)
        let window = try XCTUnwrap(document.windowControllers.first?.window)
        var onScreen = false
        var sheets = 0
        document.isOnScreen = { _ in onScreen }
        document.showSheet = { _, _, _ in sheets += 1 }
        _ = model.call { try $0.debugPanic() }
        XCTAssertTrue(document.isOfferingReopen)
        XCTAssertEqual(sheets, 0, "nobody would see it")

        onScreen = true
        NotificationCenter.default.post(name: NSWindow.didDeminiaturizeNotification, object: window)
        XCTAssertEqual(sheets, 1)
        NotificationCenter.default.post(name: NSWindow.didBecomeKeyNotification, object: window)
        XCTAssertEqual(sheets, 1, "shown once")
        document.close()
    }

    /// A failed UTF-16 document shows no banner, and its details can't be
    /// opened from stale diagnostics (phase 1 review, app-2).
    func testAFailedDocumentKeepsNoBannersOrDetails() async throws {
        let url = directory.appending(path: "legacy.csv")
        var data = Data([0xFF, 0xFE])
        data.append("id\tname\r\n1\tZoë\0\r\n".data(using: .utf16LittleEndian)!)
        try data.write(to: url)
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        let content = try XCTUnwrap((document.windowControllers.first as? DocumentWindowController)?.content)
        _ = content.view
        XCTAssertNotNil(content.readOnlyBanner)
        let details = NSMenuItem(title: "", action: #selector(DocumentViewController.showDetails(_:)), keyEquivalent: "")
        XCTAssertTrue(content.validateMenuItem(details), "the NUL is an irregularity")
        _ = model.call { try $0.debugPanic() }
        XCTAssertNil(content.readOnlyBanner)
        XCTAssertTrue(content.banners.arrangedSubviews.isEmpty)
        XCTAssertFalse(content.validateMenuItem(details))
        document.close()
    }

    // MARK: Releasing the core (phase 1 review, app-10)

    /// What `CoreRelease` hands over is released off the main thread, so a
    /// core document's teardown (its watcher thread's end, its snapshot's
    /// deletion) never makes the main thread wait.
    func testCoreObjectsAreReleasedOffTheMainThread() {
        let released = expectation(description: "released")
        let thread = ReleaseThread()
        var object: ReleaseProbe? = ReleaseProbe { onMain in
            thread.onMain = onMain
            released.fulfill()
        }
        CoreRelease.later(&object)
        XCTAssertNil(object)
        CoreRelease.finish()
        wait(for: [released], timeout: 5)
        XCTAssertEqual(thread.onMain, false)
    }

    // MARK: Very wide files

    /// Column sizing reads at most `sizingFieldLimit` fields, however wide
    /// the file: 20,000 columns are sized from 5 rows, not 1,000.
    func testColumnSizingOfAVeryWideFileIsCapped() async throws {
        let columns = 20_000
        var text = (0..<columns).map { _ in "a" }.joined(separator: ",") + "\n"
        let row = (0..<columns).map { String($0 % 10) }.joined(separator: ",") + "\n"
        for _ in 0..<30 { text += row }
        let document = try open(try file("wide.csv", text))
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        // The refined sizing runs once the index is complete; let it land.
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(model.columnCount, columns)
        XCTAssertEqual(model.columnWidths.count, columns)
        XCTAssertGreaterThan(model.largestSizingRead, 0)
        XCTAssertLessThanOrEqual(model.largestSizingRead, DocumentModel.sizingFieldLimit)
        XCTAssertEqual(DocumentModel.sizingRows(wanted: 1_000, columns: columns), 5)
        XCTAssertEqual(DocumentModel.sizingRows(wanted: 1_000, columns: 12), 1_000)
        XCTAssertEqual(DocumentModel.sizingRows(wanted: 1_000, columns: 1_000_000), 1)
        XCTAssertNotNil(model.fittingWidth(column: columns - 1, visibleRows: 0..<5))
        XCTAssertEqual(model.cell(row: 29, column: columns - 1), .text("9", truncated: false))
        document.close()
    }

    // MARK: The status bar

    func testAFileReadIntoMemoryHasAStatusBarNote() {
        var status = StatusSummary(
            rows: 3, columns: 2, indexing: false, fractionIndexed: 1, delimiter: .comma, lineEnding: .lf,
            encoding: .utf8, encodingSource: .guess, header: true, headerSource: .guess, readOnly: false
        )
        XCTAssertEqual(StatusText.segments(status), ["3 rows × 2 columns", "Comma", "LF", "UTF-8"])
        status.storage = .memory
        XCTAssertEqual(StatusText.segments(status), ["3 rows × 2 columns", "Comma", "LF", "UTF-8", "Read into memory"])
        XCTAssertTrue(StatusText.help(status).contains("read the whole file into memory"))
    }

    // MARK: The header row (ADR-0002 question 13)

    /// Task 2.0a review, item 6c: a grid's read ahead still under way when
    /// the file is read another way (the Header row, Treat As, Reload) is
    /// thrown away when it comes back, and the rows are read again the new
    /// way.
    func testAReadAheadUnderWayWhenTheFileIsReadAgainIsDropped() async throws {
        let (url, _) = try bigFile("ahead.csv", bytes: 300_000)
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertTrue(model.interpretation.header)
        for change in ["the header row", "Treat As", "Reload"] {
            XCTAssertNil(model.cachedCell(row: 640, column: 0), "\(change): row 640 isn't read yet")
            let back = model.readsAheadBack
            CellTileCache.suspendReadsAhead()
            model.readAhead(rows: 640..<704, columns: 0..<4)
            switch change {
            case "the header row": model.setHeaderRow(false)
            case "Treat As": model.treatAs(.semicolon)
            default: try model.reload()
            }
            CellTileCache.resumeReadsAhead()
            try await waitUntil("\(change): the read is back") { model.readsAheadBack > back }
            try await waitUntil("\(change): indexed again") { model.isIndexComplete && model.loadedRowCount > 704 }
            XCTAssertNil(model.cachedCell(row: 640, column: 0), "\(change): the read of the old reading was kept")
        }
        document.close()
    }

    func testTheHeaderRowToggleReadsTheFileAgain() async throws {
        let url = try file("readings.csv", "2025-03-01T00:00:00Z,S-01,18.2\n2025-03-01T00:10:00Z,S-02,18.3\n2025-03-01T00:20:00Z,S-03,18.1\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertFalse(model.interpretation.header)
        XCTAssertEqual(model.headerTitle(column: 0), HeaderTitle(text: "1", style: .number))
        XCTAssertEqual(model.rowCount, 3)
        XCTAssertTrue(model.isNumeric(column: 2))
        XCTAssertEqual(StatusText.segments(model.status).last, "No header row detected")
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        XCTAssertFalse(controller.content.statusBar.headerToggle.isHidden)

        controller.content.statusBar.headerToggle.performClick(nil)
        XCTAssertTrue(model.interpretation.header)
        XCTAssertEqual(model.interpretation.headerSource, .user)
        XCTAssertEqual(model.headerTitle(column: 1), HeaderTitle(text: "S-01", style: .name))
        try await waitUntil("indexed again") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 2)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("S-02", truncated: false))
        XCTAssertTrue(controller.content.statusBar.headerToggle.isHidden)
        document.close()
    }

    func testAColumnOnlyLongRowsHaveIsTitledAndDimmed() async throws {
        let url = try file("ragged.csv", "a,b\n1,2\n3,4,5\n6,7\n")
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        XCTAssertEqual(model.columnCount, 3)
        XCTAssertEqual(model.headerTitle(column: 2), HeaderTitle(text: "Column 3", style: .extra))
        XCTAssertEqual(model.cell(row: 0, column: 2), .missing)
        XCTAssertEqual(model.cell(row: 1, column: 2), .text("5", truncated: false))
        XCTAssertEqual(model.status.columns, 2)
        document.close()
    }

    // MARK: UTF-16 (mockup 06a)

    func testUTF16FilesOpenReadOnlyWithABanner() async throws {
        let url = directory.appending(path: "legacy.csv")
        var data = Data([0xFF, 0xFE])
        data.append("id\tname\r\n1\tZoë\r\n".data(using: .utf16LittleEndian)!)
        try data.write(to: url)
        let document = try open(url)
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertTrue(model.isReadOnly)
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        XCTAssertEqual(controller.content.banners.arrangedSubviews.count, 1)
        XCTAssertEqual(
            StatusText.segments(model.status),
            ["1 row × 2 columns", "Tab", "CRLF", "UTF-16 LE (BOM)", "Read-only"]
        )
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Zoë", truncated: false))
        document.close()
    }

    // MARK: The window

    /// However small the window, and with the find bar, the inspector and
    /// banners showing, the grid keeps its header and a few rows (phase 1
    /// review, app-6).
    func testTheGridKeepsItsRoomInASmallWindowFullOfChrome() throws {
        let document = try open(try file("orders.csv", "order_id,customer\nA-1,Sable Optics\nA-2,Loire Provisions\n"))
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        let content = controller.content
        window.setContentSize(window.contentMinSize)
        content.showFindBar()
        content.setInspectorShown(true)
        for key in ["a", "b", "c"] {
            let banner = BannerView(kind: .info, message: "A banner \(key)", buttonTitle: nil, target: nil, action: nil)
            content.banners.addArrangedSubview(banner)
        }
        content.updateWindowMinimum()
        let chrome = FindBarView.height + 3 * BannerView.minimumHeight + CellInspectorView.height + StatusBarView.height
        XCTAssertEqual(window.contentMinSize.height, chrome + DocumentViewController.gridMinimumHeight)

        // The window grew to hold everything.
        content.view.layoutSubtreeIfNeeded()
        XCTAssertEqual(content.view.frame.width, window.contentRect(forFrameRect: window.frame).width, accuracy: 0.5, "no wider than the window")
        let height = window.contentRect(forFrameRect: window.frame).height
        XCTAssertGreaterThanOrEqual(height, window.contentMinSize.height)
        XCTAssertEqual(content.view.frame.height, height, accuracy: 0.5, "the content fits the window")
        XCTAssertGreaterThanOrEqual(content.grid.frame.height, DocumentViewController.gridMinimumHeight)
        XCTAssertEqual(content.inspector.frame.height, CellInspectorView.height)
        // The content view isn't flipped: the banners are above the grid.
        XCTAssertGreaterThanOrEqual(content.banners.frame.minY, content.grid.frame.maxY - 0.5, "nothing overlaps the grid")

        // Hiding the inspector gives its room to the grid, and lowers the
        // minimum.
        let before = content.grid.frame.height
        content.setInspectorShown(false)
        content.view.layoutSubtreeIfNeeded()
        XCTAssertEqual(content.inspector.frame.height, 0)
        XCTAssertEqual(content.grid.frame.height, before + CellInspectorView.height, accuracy: 0.5)
        XCTAssertEqual(window.contentMinSize.height, chrome - CellInspectorView.height + DocumentViewController.gridMinimumHeight)
        document.close()
    }

    /// The whole window draws: rows in the grid, titles in the header.
    func testTheWindowDrawsTheGrid() throws {
        let url = try file("orders.csv", "order_id,customer\nA-100231,Sable Optics\nA-100232,Loire Provisions\n")
        let document = try open(url)
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        window.appearance = NSAppearance(named: .aqua)
        let view = try XCTUnwrap(window.contentView)
        view.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: view.bounds))
        view.cacheDisplay(in: view.bounds, to: rep)
        // The first data row's first cell, below the header, right of the
        // gutter.
        let grid = controller.content.grid
        let cell = grid.convert(NSRect(x: grid.scrollView.frame.minX, y: GridMetrics.headerHeight, width: 80, height: 22), to: view)
        let flipped = NSRect(x: cell.minX, y: view.bounds.height - cell.maxY, width: cell.width, height: cell.height)
        let scale = CGFloat(rep.pixelsWide) / view.bounds.width
        var dark = 0
        for y in Int(flipped.minY * scale)..<Int(flipped.maxY * scale) {
            for x in Int(flipped.minX * scale)..<Int(flipped.maxX * scale) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.brightnessComponent < 0.5 { dark += 1 }
            }
        }
        XCTAssertGreaterThan(dark, 20, "the first cell has text")
        document.close()
    }
}

/// Calls back from its `deinit` with whether it ran on the main thread.
private final class ReleaseProbe: Sendable {
    let onDeinit: @Sendable (Bool) -> Void

    init(_ onDeinit: @escaping @Sendable (Bool) -> Void) {
        self.onDeinit = onDeinit
    }

    deinit {
        onDeinit(Thread.isMainThread)
    }
}

/// Where a `ReleaseProbe` was released.
private final class ReleaseThread: @unchecked Sendable {
    // @unchecked: written once on the release queue before the expectation
    // is fulfilled, and read after waiting for it.
    var onMain: Bool?
}
