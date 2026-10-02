import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Files on network shares (ADR-0009, task 2.0) in the sandboxed app. There
/// is no real share here: the core's test hooks open the file as if it were
/// on one (`debugOpenDocumentSimulatingShare`), slow or failing. The tests
/// open documents the way Finder and File ▸ Open do, through
/// `NSDocumentController`, which reads them off the main thread.
///
/// They check that first paint reads the share off the main thread and the
/// share is never read on it, that rows not yet copied show as loading and
/// fill in, that a share that stops answering shows the disconnected banner
/// and one whose file was deleted elsewhere says so, and that a Reload
/// opens the file off the main thread too.
@MainActor
final class NetworkShareTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-share-\(UUID().uuidString)")
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
        DocumentModel.openForTesting = nil
        DocumentModel.shareRecheckInterval = .seconds(5)
        OpeningIndicator.delay = .milliseconds(500)
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        CoreRelease.finish()
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    /// Which threads the core's documents were opened on, in order.
    private final class Opens: @unchecked Sendable {
        // @unchecked: `threads` is only touched with `lock` held.
        private let lock = NSLock()
        private var threads: [Bool] = []

        func record() {
            let onMain = Thread.isMainThread
            lock.withLock { threads.append(onMain) }
        }

        /// For each open, whether it was on the main thread.
        var onMainThread: [Bool] { lock.withLock { threads } }
    }

    /// Opens files as if on a network share whose reads take `readDelay`
    /// milliseconds and fail as `failure` says, copied in chunks of
    /// `chunkBytes`, with the copy's reads past `holdAt` waiting for
    /// `debugShareRelease`, recording where each was opened.
    private func simulateShare(chunkBytes: UInt32 = 32_768, readDelay: UInt32 = 0, failure: SimulatedShareFailure? = nil, holdAt: UInt64? = nil) -> Opens {
        let opens = Opens()
        DocumentModel.openForTesting = { path, environment, options, observer in
            opens.record()
            return try debugOpenDocumentSimulatingShare(
                path: path,
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                chunkBytes: chunkBytes,
                readDelayMs: readDelay,
                failure: failure,
                holdAt: holdAt
            )
        }
        return opens
    }

    /// Opens `url` through `NSDocumentController` (Leal's
    /// `DocumentController`, which opens the core's document off the main
    /// thread), and makes its window without showing it.
    private func open(_ url: URL) async throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let opened: NSDocument = try await withCheckedThrowingContinuation { continuation in
            NSDocumentController.shared.openDocument(withContentsOf: url, display: false) { document, _, error in
                if let document {
                    continuation.resume(returning: document)
                } else {
                    continuation.resume(throwing: error ?? NSError(domain: "NetworkShareTests", code: 1))
                }
            }
        }
        let document = try XCTUnwrap(opened as? CSVDocument)
        if document.windowControllers.isEmpty {
            document.makeWindowControllers()
        }
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        controller.window?.appearance = NSAppearance(named: .aqua)
        _ = controller.window
        return (document, try XCTUnwrap(document.model), controller.content)
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

    /// `rows` data rows after a header, each its number and `prefix` with it.
    private func file(_ name: String, rows: Int, prefix: String = "name") throws -> URL {
        var text = "id,name\n"
        for i in 0..<rows { text += "\(i),\(prefix) \(i)\n" }
        let url = directory.appending(path: name)
        try Data(text.utf8).write(to: url)
        return url
    }

    /// How many times the document's share was read on the main thread
    /// (ADR-0009 says never).
    private func mainThreadReads(_ model: DocumentModel) -> UInt64? {
        model.backgroundHandle()?.debugShareReadsOnMainThread()
    }

    /// Scrolls the grid to its last rows and returns them.
    private func scrollToEnd(_ grid: GridContainerView) -> Range<Int> {
        grid.layoutSubtreeIfNeeded()
        let clip = grid.scrollView.contentView
        clip.scroll(to: NSPoint(x: 0, y: max(0, grid.gridView.frame.height - clip.bounds.height)))
        grid.scrollView.reflectScrolledClipView(clip)
        let visible = grid.gridView.visibleRect
        let first = Int(visible.minY / GridMetrics.rowHeight)
        let last = Int((visible.maxY / GridMetrics.rowHeight).rounded(.up))
        return first..<min(last, grid.dataSource?.rowCount ?? 0)
    }

    /// Draws the grid's visible cells offscreen and counts the pixels of
    /// text in them: a loading (skeleton) row has none.
    private func textPixels(_ grid: GridContainerView) throws -> Int {
        let view = grid.gridView
        let rect = view.visibleRect
        let rep = try XCTUnwrap(view.bitmapImageRepForCachingDisplay(in: rect))
        view.cacheDisplay(in: rect, to: rep)
        var count = 0
        for y in stride(from: 0, to: rep.pixelsHigh, by: 2) {
            for x in stride(from: 0, to: rep.pixelsWide, by: 2) {
                if let color = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), color.brightnessComponent < 0.5 {
                    count += 1
                }
            }
        }
        return count
    }

    // MARK: Opening and loading rows

    /// A share whose copy is held part-way (the core's test hook, so this
    /// doesn't race the copy): the file is opened off the main thread; the
    /// window shows the first screen at once and the rest as loading rows,
    /// drawn as skeletons; once the copy goes on, those rows fill in and the
    /// grid asks to redraw; the share is never read on the main thread, even
    /// while the grid draws the loading rows.
    func testRowsNotCopiedYetShowAsLoadingThenFillIn() async throws {
        let rows = 40_000
        let url = try file("share.csv", rows: rows)
        let opens = simulateShare(chunkBytes: 32_768, holdAt: 200_000)
        let (document, model, content) = try await open(url)
        XCTAssertEqual(opens.onMainThread, [false], "first paint read the share off the main thread")
        XCTAssertTrue(model.isOnNetworkShare)
        // Everything up to the hold is copied and indexed, and nothing past
        // it: the copy is waiting.
        try await waitUntil("copied to the hold") { model.loadedRowCount > 10_000 }
        try await Task.sleep(for: .milliseconds(100))
        let loaded = model.loadedRowCount
        XCTAssertLessThan(loaded, rows / 2)
        XCTAssertEqual(model.storage, .reading)
        XCTAssertFalse(model.isIndexComplete)
        XCTAssertTrue(StatusText.segments(model.status).contains("Reading from the network"))
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))

        // The last rows aren't copied yet: they are loading rows, drawn as
        // skeletons, with no text.
        let grid = content.grid
        let end = scrollToEnd(grid)
        XCTAssertFalse(end.isEmpty)
        for row in end {
            XCTAssertEqual(model.cell(row: row, column: 1), .notLoaded, "row \(row)")
        }
        XCTAssertEqual(try textPixels(grid), 0, "skeleton rows only")
        XCTAssertEqual(mainThreadReads(model), 0, "drawing loading rows doesn't read the share")

        // The copy goes on: the rows fill in, and the grid asks to redraw.
        grid.gridView.needsDisplay = false
        model.backgroundHandle()?.debugShareRelease()
        try await waitUntil("copied and indexed") { model.isIndexComplete }
        XCTAssertTrue(grid.gridView.needsDisplay, "the rows' arrival asks the grid to redraw")
        XCTAssertEqual(model.rowCount, rows)
        let filled = scrollToEnd(grid)
        XCTAssertEqual(filled.upperBound, rows)
        for row in filled {
            XCTAssertEqual(model.cell(row: row, column: 1), .text("name \(row)", truncated: false), "row \(row)")
        }
        XCTAssertGreaterThan(try textPixels(grid), 100, "the rows are drawn with their text")
        try await waitUntil("worked from the copy") { model.storage == .copy }
        XCTAssertTrue(StatusText.segments(model.status).contains("Working from a copy"))
        XCTAssertTrue(document.canSave)
        XCTAssertEqual(mainThreadReads(model), 0)
        XCTAssertEqual(opens.onMainThread, [false])
        document.close()
    }

    /// An open that takes longer than the delay shows "Opening…" until it
    /// is done (task 2.0 review); a quick one shows nothing.
    func testASlowOpenShowsOpening() async throws {
        OpeningIndicator.delay = .milliseconds(50)
        let url = try file("slow.csv", rows: 100)
        _ = simulateShare(readDelay: 400)
        var seen = false
        let opening = Task { @MainActor in
            while !Task.isCancelled {
                if OpeningIndicator.shown.contains("slow.csv") { seen = true }
                try? await Task.sleep(for: .milliseconds(10))
            }
        }
        let (document, _, _) = try await open(url)
        opening.cancel()
        XCTAssertTrue(seen, "the indicator showed while the open waited")
        XCTAssertEqual(OpeningIndicator.shown, [], "and went once it was done")
        document.close()

        OpeningIndicator.delay = .seconds(5)
        let quick = try file("quick.csv", rows: 100)
        _ = simulateShare()
        let (other, _, _) = try await open(quick)
        XCTAssertEqual(OpeningIndicator.shown, [])
        other.close()
    }

    // MARK: Errors (ADR-0009)

    /// A share that stops answering (a network error that doesn't pass):
    /// after the retries, the existing "drive disconnected" state and
    /// banner, Save off and Save As offered. When the share answers again,
    /// the check on a volume mounting reconnects it and the copy completes.
    func testAShareThatStopsAnsweringShowsTheDisconnectedBanner() async throws {
        let rows = 20_000
        let url = try file("share.csv", rows: rows)
        let opens = simulateShare(failure: SimulatedShareFailure(at: 100_000, errno: ETIMEDOUT, times: nil))
        let (document, model, content) = try await open(url)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        // The index's end reaches the window after the state does.
        try await waitUntil("the index stopped") { model.isIndexComplete }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.disconnected)
        XCTAssertEqual(banner.button?.title, "Save As…")
        XCTAssertTrue(StatusText.segments(model.status).contains("Drive disconnected"))
        XCTAssertFalse(document.canSave)
        XCTAssertLessThan(model.loadedRowCount, rows)
        XCTAssertEqual(model.rowCount, model.loadedRowCount, "no loading rows for rows that won't come")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))

        // Back: the app looks again when a volume mounts.
        model.backgroundHandle()?.debugSimulateDriveBack()
        NSWorkspace.shared.notificationCenter.post(name: NSWorkspace.didMountNotification, object: nil)
        try await waitUntil("reconnected and copied") { model.storage == .copy && model.isIndexComplete }
        XCTAssertEqual(model.rowCount, rows)
        XCTAssertTrue(document.canSave)
        XCTAssertNil(content.driveBanner)
        XCTAssertEqual(mainThreadReads(model), 0)
        XCTAssertEqual(opens.onMainThread, [false])
        document.close()
    }

    /// A share whose SMB session comes back by itself fires no mount
    /// notification: while disconnected, the model looks again now and then
    /// (task 2.0 review), and the copy carries on with nothing posted.
    func testADisconnectedShareRecoversByItselfWithoutANotification() async throws {
        DocumentModel.shareRecheckInterval = .milliseconds(100)
        let rows = 20_000
        let url = try file("share.csv", rows: rows)
        _ = simulateShare(failure: SimulatedShareFailure(at: 100_000, errno: ECONNRESET, times: nil))
        let (document, model, _) = try await open(url)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        model.backgroundHandle()?.debugSimulateDriveBack()
        try await waitUntil("back and copied") { model.storage == .copy && model.isIndexComplete }
        XCTAssertEqual(model.rowCount, rows)
        XCTAssertTrue(document.canSave)
        document.close()
    }

    /// A share that reconnects and then fails at the same place each time (a
    /// bad block, not a share coming and going): after three such failures
    /// the periodic check stops, and the document stays disconnected with
    /// its banner until an app activation (or a mount, or Reload) tries again
    /// (task 2.0 re-review).
    func testASharePeriodicCheckBacksOffAfterTheSameFailureThreeTimes() async throws {
        DocumentModel.shareRecheckInterval = .milliseconds(50)
        let url = try file("share.csv", rows: 20_000)
        // Not "away" (it has a count), so each check reconnects, and each
        // copy then fails at the same chunk.
        _ = simulateShare(failure: SimulatedShareFailure(at: 100_000, errno: EACCES, times: 1_000))
        let (document, model, content) = try await open(url)
        // Three failures in a row at the same place (counted once the window
        // has seen each reading, so one more reading may slip in), then it
        // settles.
        try await waitUntil("failed and reconnected") {
            model.generation >= 2 && model.storage == .disconnected && model.isIndexComplete
        }
        try await Task.sleep(for: .milliseconds(500))
        let settled = model.generation
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(model.generation, settled, "no more checks once backed off")
        XCTAssertLessThanOrEqual(settled, 4, "backed off after a few identical failures")
        XCTAssertEqual(model.storage, .disconnected)
        XCTAssertEqual(content.driveBanner?.message, DiagnosticsText.disconnected)

        // Activation tries again.
        NotificationCenter.default.post(name: NSApplication.didBecomeActiveNotification, object: nil)
        try await waitUntil("checked again") { model.generation > settled }
        document.close()
    }

    /// `ESTALE` with the same file still at its path: only the share's
    /// handle went stale. Disconnected, not deleted.
    func testAStaleHandleIsADisconnectionNotADeletion() async throws {
        let url = try file("share.csv", rows: 20_000)
        _ = simulateShare(failure: SimulatedShareFailure(at: 100_000, errno: ESTALE, times: nil))
        let (document, model, content) = try await open(url)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        try await waitUntil("its banner") { content.driveBanner != nil }
        XCTAssertEqual(content.driveBanner?.message, DiagnosticsText.disconnected)
        XCTAssertFalse(StatusText.segments(model.status).contains("Deleted"))
        document.close()
    }

    /// A file on a share replaced by another computer mid-copy (the look on
    /// activation finds it changed): the rest of the copy would be the new
    /// version, so it is changed while reading. The banner offers Reload,
    /// and the status bar says both "Changed while reading" and "Changed on
    /// disk" (task 2.0 review).
    func testAShareFileReplacedMidCopyOffersReload() async throws {
        // Written in place (the same inode), and saved the safe way (a new
        // file renamed over it: a new inode).
        for options: Data.WritingOptions in [[], .atomic] {
            try await replaceMidCopy(writing: options)
        }
    }

    private func replaceMidCopy(writing options: Data.WritingOptions) async throws {
        let url = try file("share.csv", rows: 20_000)
        _ = simulateShare(holdAt: 200_000)
        let (document, model, content) = try await open(url)
        var text = "id,name\n"
        for i in 0..<20_001 { text += "\(i),new \(i)\n" }
        try Data(text.utf8).write(to: url, options: options)
        await model.checkOriginal()?.value
        try await waitUntil("changed while reading") { model.changedOnDisk }
        try await waitUntil("its banner") { content.driveBanner?.message == DiagnosticsText.changedWhileReading }
        XCTAssertEqual(content.driveBanner?.button?.title, "Reload")
        let segments = StatusText.segments(model.status)
        XCTAssertTrue(segments.contains("Changed while reading"), "\(segments)")
        XCTAssertTrue(segments.contains("Changed on disk"), "\(segments)")
        XCTAssertFalse(document.canSave)
        model.backgroundHandle()?.debugShareRelease()
        document.close()
    }

    /// `ESTALE` from the share with nothing at the file's path, its folder
    /// still there: another computer deleted the file. The window says so
    /// (not "disconnected"), keeps the rows it read, shows no loading rows
    /// for the rest, and Save is off.
    func testAFileDeletedOnItsShareSaysSo() async throws {
        let rows = 20_000
        let url = try file("share.csv", rows: rows)
        _ = simulateShare(failure: SimulatedShareFailure(at: 100_000, errno: ESTALE, times: nil), holdAt: 100_000)
        let (document, model, content) = try await open(url)
        try FileManager.default.removeItem(at: url)
        model.backgroundHandle()?.debugShareRelease()
        try await waitUntil("deleted") { model.storage == .deleted }
        try await waitUntil("the index stopped") { model.isIndexComplete }
        try await waitUntil("its banner") { content.driveBanner != nil }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, FileBanner.deletedWhileReadingMessage)
        XCTAssertEqual(banner.button?.title, "Save As…")
        XCTAssertNil(banner.secondaryButton)
        XCTAssertTrue(StatusText.segments(model.status).contains("Deleted"))
        XCTAssertFalse(StatusText.segments(model.status).contains("Drive disconnected"))
        XCTAssertFalse(document.canSave)
        XCTAssertLessThan(model.loadedRowCount, rows)
        XCTAssertEqual(model.rowCount, model.loadedRowCount)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))
        XCTAssertEqual(mainThreadReads(model), 0)
        document.close()
    }

    // MARK: Reload

    /// **Reload** of a file on a share opens it off the main thread too,
    /// then shows the file as it is now.
    func testReloadOfAFileOnAShareOpensItOffTheMainThread() async throws {
        let url = try file("share.csv", rows: 3_000)
        let opens = simulateShare()
        let (document, model, content) = try await open(url)
        try await waitUntil("copied") { model.isIndexComplete && model.storage == .copy }
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))

        _ = try file("share.csv", rows: 5_000, prefix: "new")
        content.reloadFromDisk(nil)
        let reloading = try XCTUnwrap(content.reloading, "Reload runs in the background")
        // The re-readings are off meanwhile.
        XCTAssertFalse(model.canReinterpret)
        XCTAssertFalse(content.validateMenuItem(NSMenuItem(title: "", action: #selector(DocumentViewController.reloadFromDisk(_:)), keyEquivalent: "")))
        await reloading.value
        XCTAssertTrue(model.canReinterpret)
        XCTAssertEqual(opens.onMainThread, [false, false], "both opens were off the main thread")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("new 0", truncated: false))
        try await waitUntil("copied again") { model.isIndexComplete && model.storage == .copy }
        XCTAssertEqual(model.rowCount, 5_000)
        XCTAssertTrue(model.isOnNetworkShare)
        XCTAssertEqual(mainThreadReads(model), 0)
        XCTAssertEqual(document.fileURL, url)
        document.close()
    }
}
