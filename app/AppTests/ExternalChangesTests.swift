import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 1.9 against the real core, hosted (and sandboxed) in Leal.app: the
/// file watched for changes made elsewhere, the "This file changed on
/// disk" banner with Reload and Keep Editing, deleted and moved files, a
/// change while reading from a drive that can't make a snapshot, and a
/// disconnected drive coming back (simulated with the core's test hooks).
@MainActor
final class ExternalChangesTests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-external-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let environment = DocumentEnvironment(
            scheduler: try Scheduler(),
            temp: TempLocations(
                scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
                recordsDir: directory.appending(path: "records").path(percentEncoded: false)
            )
        )
        savedEnvironment = CSVDocument.environment
        CSVDocument.environment = { environment }
    }

    override func tearDown() async throws {
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        DocumentModel.openForTesting = nil
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private func file(_ name: String, _ text: String) throws -> URL {
        let url = directory.appending(path: name)
        try Data(text.utf8).write(to: url)
        return url
    }

    private func open(_ url: URL) throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        return (document, try XCTUnwrap(document.model), controller.content)
    }

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

    /// `rows` data rows after a header, each `prefix` and its number.
    private func text(rows: Int, prefix: String) -> String {
        var text = "id,name\n"
        for i in 0..<rows { text += "\(i),\(prefix) \(i)\n" }
        return text
    }

    /// Replaces the file's contents the way most apps save: a new file
    /// renamed over the old one.
    private func save(_ text: String, to url: URL) throws {
        try Data(text.utf8).write(to: url, options: .atomic)
    }

    private func menuItem(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }

    // MARK: Changed elsewhere

    func testAChangeOnDiskShowsTheBannerWithReloadAndKeepEditing() async throws {
        let url = try file("people.csv", text(rows: 20, prefix: "old"))
        let (document, model, content) = try open(url)
        XCTAssertNil(content.driveBanner)
        XCTAssertEqual(model.original.state, .unchanged)

        try save(text(rows: 30, prefix: "new"), to: url)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, FileBanner.changedMessage)
        XCTAssertEqual(banner.button?.title, "Reload")
        XCTAssertEqual(banner.secondaryButton?.title, "Keep Editing")
        XCTAssertEqual(content.banners.arrangedSubviews.first, banner, "the file's banner comes first")
        XCTAssertTrue(StatusText.segments(model.status).contains("Changed on disk"))
        // Leal still shows what it opened, and Save (phase 2) asks first.
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("old 0", truncated: false))
        XCTAssertTrue(document.canSave)
        XCTAssertTrue(model.original.diverged)
        document.close()
    }

    func testReloadShowsTheNewRowsAndKeepsTheSelection() async throws {
        let url = try file("people.csv", text(rows: 200, prefix: "old"))
        let (document, model, content) = try open(url)
        try await waitUntil("indexed") { model.isIndexComplete }
        content.grid.select(CellPosition(row: 120, column: 1))
        let origin = content.grid.scrollView.contentView.bounds.origin
        XCTAssertGreaterThan(origin.y, 0, "scrolled to show row 120")

        try save(text(rows: 300, prefix: "new"), to: url)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        try XCTUnwrap(content.driveBanner?.button).performClick(nil)

        XCTAssertNil(content.driveBanner, "the new snapshot is what's on disk")
        XCTAssertEqual(model.original.state, .unchanged)
        XCTAssertFalse(model.original.diverged)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("new 0", truncated: false))
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 120, column: 1))
        XCTAssertEqual(content.grid.scrollView.contentView.bounds.origin.y, origin.y, accuracy: 0.5)
        XCTAssertFalse(StatusText.segments(model.status).contains("Changed on disk"))
        try await waitUntil("indexed again") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 300)
        XCTAssertEqual(model.cell(row: 299, column: 1), .text("new 299", truncated: false))
        // And it is watched again: a second change is seen too.
        try save(text(rows: 5, prefix: "newer"), to: url)
        try await waitUntil("the second change is seen") { model.original.state == .changed }
        XCTAssertNotNil(content.driveBanner)
        document.close()
    }

    func testKeepEditingKeepsTheSnapshot() async throws {
        let url = try file("people.csv", text(rows: 20, prefix: "old"))
        let (document, model, content) = try open(url)
        try save(text(rows: 3, prefix: "new"), to: url)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        try XCTUnwrap(content.driveBanner?.secondaryButton).performClick(nil)

        XCTAssertNil(content.driveBanner)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("old 0", truncated: false))
        XCTAssertEqual(model.cell(row: 19, column: 1), .text("old 19", truncated: false))
        XCTAssertTrue(model.original.diverged, "remembered for Save's check (phase 2)")
        XCTAssertTrue(StatusText.segments(model.status).contains("Changed on disk"), "the status bar keeps a note")
        // File ▸ Reload from Disk still works after Keep Editing.
        XCTAssertTrue(content.validateMenuItem(menuItem(#selector(DocumentViewController.reloadFromDisk(_:)))))
        // A later change doesn't bring the banner back: the user chose to
        // keep this version.
        try save(text(rows: 4, prefix: "newer"), to: url)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertNil(content.driveBanner)
        content.reloadFromDisk(nil)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("newer 0", truncated: false))
        document.close()
    }

    /// How NSDocument apps save: `FileManager.replaceItemAt` swaps a new
    /// file in through a replacement folder. That is a change, the document
    /// stays where it is, and it is never reported deleted.
    func testASafeSaveByAnotherAppIsAChangeNotAMove() async throws {
        let url = try file("people.csv", text(rows: 20, prefix: "old"))
        let (document, model, content) = try open(url)
        let staging = try FileManager.default.url(for: .itemReplacementDirectory, in: .userDomainMask, appropriateFor: url, create: true)
        let new = staging.appending(path: "people.csv")
        try Data(text(rows: 30, prefix: "new").utf8).write(to: new)
        _ = try FileManager.default.replaceItemAt(url, withItemAt: new)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(model.original.state, .changed, "not deleted afterwards")
        XCTAssertEqual(model.url.standardizedFileURL.path, url.standardizedFileURL.path)
        XCTAssertEqual(document.fileURL?.lastPathComponent, "people.csv")
        XCTAssertFalse(document.fileURL?.pathComponents.contains("TemporaryItems") ?? true)
        XCTAssertEqual(content.driveBanner?.message, FileBanner.changedMessage)
        content.reloadFromDisk(nil)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("new 0", truncated: false))
        try? FileManager.default.removeItem(at: staging)
        document.close()
    }

    func testADeletedFileShowsItsBannerAndCantBeReloaded() async throws {
        let url = try file("people.csv", text(rows: 5, prefix: "old"))
        let (document, model, content) = try open(url)
        try FileManager.default.removeItem(at: url)
        try await waitUntil("the deletion is seen") { model.original.state == .deleted }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, FileBanner.deletedMessage)
        XCTAssertEqual(banner.button?.title, "Save As…")
        XCTAssertEqual(banner.secondaryButton?.title, "Keep Editing")
        XCTAssertTrue(StatusText.segments(model.status).contains("Deleted"))
        XCTAssertFalse(content.validateMenuItem(menuItem(#selector(DocumentViewController.reloadFromDisk(_:)))))
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("old 0", truncated: false))

        // A file written at its path again is a change, which can be
        // reloaded.
        try Data(text(rows: 2, prefix: "back").utf8).write(to: url)
        try await waitUntil("the new file is seen") { model.original.state == .changed }
        XCTAssertEqual(content.driveBanner?.message, FileBanner.changedMessage)
        content.reloadFromDisk(nil)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("back 0", truncated: false))
        document.close()
    }

    func testAMovedFileIsFollowed() async throws {
        let url = try file("people.csv", text(rows: 5, prefix: "old"))
        let (document, model, content) = try open(url)
        let moved = directory.appending(path: "renamed.csv")
        try FileManager.default.moveItem(at: url, to: moved)
        try await waitUntil("the move is followed") { model.url.lastPathComponent == "renamed.csv" }
        XCTAssertEqual(document.fileURL?.lastPathComponent, "renamed.csv")
        XCTAssertEqual(model.original.state, .unchanged)
        XCTAssertNil(content.driveBanner, "only its name changed")
        // Reload opens it where it is now.
        try Data(text(rows: 2, prefix: "new").utf8).write(to: moved)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        content.reloadFromDisk(nil)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("new 0", truncated: false))
        document.close()
    }

    /// Reload goes through the `NSDocument`, which learns the modification
    /// date of the file Leal now shows, so phase 2's save check won't take
    /// the accepted change for another app's. A second `read(from:)`, as
    /// `revert(toContentsOf:ofType:)` does, reloads the window's own model
    /// instead of leaving the window bound to a closed one (phase 1 review,
    /// app-3).
    func testReloadKeepsTheDocumentInStepAndAReadAgainKeepsTheModel() async throws {
        let url = try file("people.csv", text(rows: 20, prefix: "old"))
        let (document, model, content) = try open(url)
        try save(text(rows: 30, prefix: "new"), to: url)
        let later = Date(timeIntervalSince1970: (Date().timeIntervalSince1970 + 3_600).rounded(.down))
        try FileManager.default.setAttributes([.modificationDate: later], ofItemAtPath: url.path(percentEncoded: false))
        try await waitUntil("the change is seen") { model.original.state == .changed }
        try XCTUnwrap(content.driveBanner?.button).performClick(nil)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("new 0", truncated: false))
        XCTAssertEqual(try XCTUnwrap(document.fileModificationDate).timeIntervalSince1970, later.timeIntervalSince1970, accuracy: 0.001)

        try save(text(rows: 5, prefix: "newer"), to: url)
        try document.revert(toContentsOf: url, ofType: "public.comma-separated-values-text")
        XCTAssertTrue(document.model === model, "the same model")
        XCTAssertTrue(content.model === model, "the window still shows it")
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("newer 0", truncated: false))
        try await waitUntil("indexed again") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 5)
        XCTAssertEqual(content.grid.gridView.frame.height, max(CGFloat(5) * GridMetrics.rowHeight, content.grid.scrollView.contentSize.height))
        document.close()
    }

    /// The lock glyph follows the file: a Reload into UTF-16 adds it, and a
    /// Reload out of it takes it away (phase 1 review, app-4).
    func testReloadIntoAndOutOfUTF16MovesTheLock() async throws {
        let url = try file("legacy.csv", "id\tname\r\n1\tZoë\r\n")
        let (document, model, content) = try open(url)
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        XCTAssertNil(controller.lock)

        var utf16 = Data([0xFF, 0xFE])
        utf16.append(try XCTUnwrap("id\tname\r\n1\tZoë\r\n".data(using: .utf16LittleEndian)))
        try utf16.write(to: url, options: .atomic)
        try await waitUntil("the change is seen") { model.original.state == .changed }
        content.reloadFromDisk(nil)
        XCTAssertTrue(model.isReadOnly)
        let lock = try XCTUnwrap(controller.lock)
        XCTAssertTrue(controller.window?.titlebarAccessoryViewControllers.contains(lock) ?? false)
        XCTAssertNotNil(content.readOnlyBanner)

        try save("id\tname\r\n1\tZoë\r\n", to: url)
        try await waitUntil("the second change is seen") { model.original.state == .changed }
        content.reloadFromDisk(nil)
        XCTAssertFalse(model.isReadOnly)
        XCTAssertNil(controller.lock)
        XCTAssertFalse(controller.window?.titlebarAccessoryViewControllers.contains(lock) ?? true)
        XCTAssertNil(content.readOnlyBanner)
        document.close()
    }

    func testAReloadThatFailsKeepsTheDocument() async throws {
        let url = try file("people.csv", text(rows: 5, prefix: "old"))
        let (document, model, _) = try open(url)
        try FileManager.default.removeItem(at: url)
        XCTAssertThrowsError(try model.reload())
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("old 0", truncated: false))
        XCTAssertFalse(model.isFailed)
        document.close()
    }

    // MARK: Removable drives (1.1a, ADR-0006)

    /// Opens files as if on a removable drive, with `fault` part-way
    /// through the copy (the core's test hooks).
    private func simulateDrive(_ fault: SimulatedFault?) {
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentWithFault(
                path: path,
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                chunkBytes: 4096,
                fault: fault
            )
        }
    }

    /// The 1.1a re-review's obligation: after a change while reading, rows
    /// read before it are dropped, the banner offers Reload, and Save is
    /// off until then.
    func testAChangeWhileReadingDropsRowsAndOffersReload() async throws {
        simulateDrive(.change(at: 16_384))
        let contents = text(rows: 20_000, prefix: "name")
        let url = try file("exfat.csv", contents)
        // Data rows that end in the first 16 KB (copied and checked before
        // the change) and in the first 64 KB (read at first paint).
        let newlines = { (bytes: Int) in Data(contents.utf8).prefix(bytes).filter { $0 == 10 }.count - 1 }
        let (document, model, content) = try open(url)
        try await waitUntil("the change is seen") { model.changedOnDisk }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.changedWhileReading)
        XCTAssertEqual(banner.button?.title, "Reload")
        XCTAssertNil(banner.secondaryButton)
        XCTAssertFalse(document.canSave)
        // Only rows from the checked copy, not first paint's 64 KB.
        XCTAssertLessThanOrEqual(model.loadedRowCount, newlines(16_384))
        XCTAssertGreaterThan(model.loadedRowCount, newlines(16_384) - 5)
        XCTAssertLessThan(model.loadedRowCount, newlines(65_536) - 1)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("name 0", truncated: false))
        XCTAssertEqual(model.cell(row: model.loadedRowCount + 10, column: 1), .notLoaded)

        // Reload reads the file as it is now (the simulated drive is fine
        // this time).
        DocumentModel.openForTesting = nil
        banner.button?.performClick(nil)
        XCTAssertFalse(model.changedOnDisk)
        XCTAssertNil(content.driveBanner)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertTrue(document.canSave)
        XCTAssertEqual(model.rowCount, 20_000)
        document.close()
    }

    /// After a change while reading, the file can't be read another way
    /// until it is reloaded (the core refuses: the first 64 KB Leal holds
    /// may be the old version). Treat As, Reopen with Encoding and the
    /// Header row are off, in the menus and the status bar, and say to
    /// Reload first; Reload turns them back on.
    func testAChangeWhileReadingTurnsOffReadingTheFileAnotherWay() async throws {
        simulateDrive(.change(at: 16_384))
        let url = try file("exfat.csv", text(rows: 20_000, prefix: "name"))
        let (document, model, content) = try open(url)
        let treatAs = menuItem(#selector(DocumentViewController.treatAsDelimiter(_:)))
        treatAs.representedObject = DelimiterBox(.semicolon)
        let reopen = menuItem(#selector(DocumentViewController.reopenWithEncoding(_:)))
        reopen.representedObject = EncodingBox(.windows1252)
        let header = menuItem(#selector(DocumentViewController.toggleHeaderRow(_:)))
        let items = [treatAs, reopen, header]
        try await waitUntil("the change is seen") { model.changedOnDisk }
        XCTAssertFalse(model.canReinterpret)
        XCTAssertEqual(items.map { content.validateMenuItem($0) }, [false, false, false])
        XCTAssertEqual(items.map(\.toolTip), Array(repeating: StatusText.reloadFirst, count: 3))
        XCTAssertEqual(StatusText.reloadFirst, "The file changed while Leal was reading it. Reload it first.")
        let bar = content.statusBar
        XCTAssertEqual(bar.delimiterButton?.isEnabled, false)
        XCTAssertEqual(bar.delimiterButton?.toolTip, StatusText.reloadFirst)
        XCTAssertEqual(bar.encodingButton?.isEnabled, false)
        XCTAssertEqual(bar.encodingButton?.toolTip, StatusText.reloadFirst)
        XCTAssertFalse(bar.headerToggle.isEnabled)
        XCTAssertEqual(bar.headerToggle.toolTip, StatusText.reloadFirst)
        // Asked anyway (a key equivalent, a stale menu): nothing happens.
        let before = model.interpretation
        content.treatAs(.semicolon)
        content.reopen(encoding: .windows1252)
        content.toggleHeaderRow(nil)
        XCTAssertEqual(model.interpretation, before)

        // Reload: the file can be read another way again.
        DocumentModel.openForTesting = nil
        try XCTUnwrap(content.driveBanner?.button).performClick(nil)
        XCTAssertFalse(model.changedOnDisk)
        XCTAssertEqual(items.map { content.validateMenuItem($0) }, [true, true, true])
        XCTAssertEqual(items.map(\.toolTip), [nil, nil, nil])
        XCTAssertEqual(bar.delimiterButton?.isEnabled, true)
        XCTAssertEqual(bar.encodingButton?.isEnabled, true)
        XCTAssertTrue(bar.headerToggle.isEnabled)
        content.treatAs(.semicolon)
        XCTAssertEqual(model.interpretation.delimiter, .semicolon)
        document.close()
    }

    func testADisconnectedDriveThatComesBackIsReconnectedAndSaveIsAllowed() async throws {
        simulateDrive(.disconnect(at: 100_000))
        let url = try file("usb.csv", text(rows: 20_000, prefix: "name"))
        let (document, model, content) = try open(url)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        XCTAssertEqual(content.driveBanner?.message, DiagnosticsText.disconnected)
        XCTAssertFalse(document.canSave)
        let generation = model.generation

        // A check while the drive is still away changes nothing.
        await model.checkOriginal()?.value
        XCTAssertEqual(model.storage, .disconnected)
        XCTAssertEqual(model.generation, generation)

        // The drive is back, and a volume mounts.
        _ = model.call { $0.debugSimulateDriveBack() }
        NSWorkspace.shared.notificationCenter.post(name: NSWorkspace.didMountNotification, object: NSWorkspace.shared)
        try await waitUntil("reconnected") { model.storage != .disconnected }
        XCTAssertGreaterThan(model.generation, generation, "read again, to carry on copying")
        XCTAssertTrue(document.canSave)
        XCTAssertNil(content.driveBanner)
        try await waitUntil("copied and indexed") { model.isIndexComplete }
        XCTAssertEqual(model.storage, .copy)
        XCTAssertEqual(model.rowCount, 20_000)
        XCTAssertEqual(model.cell(row: 19_999, column: 1), .text("name 19999", truncated: false))
        XCTAssertTrue(StatusText.segments(model.status).contains("Working from a copy"))
        document.close()
    }

    /// A read error that stops the index (`JobFailure.Failed`, such as EIO
    /// from a failing drive) doesn't leave the window "Indexing…" for good:
    /// it shows the rows read, says so in a banner and the status bar, ends
    /// a pending ⌘↓ at the last row read, and Reload tries again (phase 1
    /// review, app-8). The core has no hook for a read error, so the test
    /// ends the index the way the model's own task would.
    func testAReadErrorThatStopsTheIndexIsShownAndOffersReload() async throws {
        let url = try file("failing.csv", text(rows: 200_000, prefix: "name"))
        let (document, model, content) = try open(url)
        content.grid.move(.lastRow)
        model.jobEnded(JobFailure.Failed(message: "Input/output error"), job: .index)

        XCTAssertTrue(model.readStopped)
        XCTAssertTrue(model.isIndexComplete, "no more rows will come")
        XCTAssertFalse(model.status.indexing)
        XCTAssertEqual(model.rowCount, model.loadedRowCount, "no skeleton rows for rows that won't come")
        XCTAssertFalse(content.grid.isJumpingToEnd)
        XCTAssertEqual(content.grid.activeCell?.row, model.rowCount - 1)
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, FileBanner.readStoppedMessage)
        XCTAssertEqual(banner.button?.title, "Reload")
        XCTAssertTrue(StatusText.segments(model.status).contains("Partly read"))
        XCTAssertFalse(StatusText.counts(model.status).hasPrefix("Indexing"))

        banner.button?.performClick(nil)
        XCTAssertFalse(model.readStopped)
        XCTAssertNil(content.driveBanner)
        try await waitUntil("indexed") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 200_000)
        XCTAssertFalse(StatusText.segments(model.status).contains("Partly read"))
        document.close()
    }

    // MARK: The banner choice and the status bar's words

    func testTheFileBannerChoiceAndOrder() {
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .unchanged, storage: .clone), [])
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .changed, storage: .clone), [.changed])
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .deleted, storage: .copy), [.deleted])
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .unavailable, storage: .copy), [])
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .unavailable, storage: .disconnected), [.disconnected])
        // Changed while the drive was away: Reload first, then (after Keep
        // Editing) the disconnection.
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .changed, storage: .disconnected), [.changed, .disconnected])
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: true, original: .changed, storage: .reading), [.changedWhileReading, .changed])
        XCTAssertTrue(FileBanner.changed.reloads)
        XCTAssertTrue(FileBanner.changedWhileReading.reloads)
        XCTAssertFalse(FileBanner.deleted.reloads)
        XCTAssertFalse(FileBanner.disconnected.reloads)
        XCTAssertEqual(Set([FileBanner.changedWhileReading, .changed, .deleted, .disconnected, .readStopped].map(\.key)).count, 5)
        XCTAssertEqual(FileBanner.applicable(changedWhileReading: false, original: .unchanged, storage: .copy, readStopped: true), [.readStopped])
        XCTAssertTrue(FileBanner.readStopped.reloads)
    }

    func testTheOriginalNotesAndTheirTooltips() {
        var status = StatusSummary(
            rows: 3, columns: 2, indexing: false, fractionIndexed: 1, delimiter: .comma, lineEnding: .lf,
            encoding: .utf8, encodingSource: .guess, header: true, headerSource: .guess, readOnly: false
        )
        XCTAssertNil(StatusText.originalNote(status))
        status.original = .changed
        XCTAssertEqual(StatusText.originalNote(status), "Changed on disk")
        XCTAssertTrue(StatusText.help(status).contains("Reload from Disk"))
        status.original = .deleted
        XCTAssertEqual(StatusText.originalNote(status), "Deleted")
        status.original = .unavailable
        status.storage = .copy
        XCTAssertEqual(StatusText.originalNote(status), "Drive not connected")
        XCTAssertTrue(StatusText.help(status).contains("Save is off until the drive is back"))
        // The storage note already says so.
        status.storage = .disconnected
        XCTAssertNil(StatusText.originalNote(status))
        status.original = .changed
        status.changedOnDisk = true
        XCTAssertNil(StatusText.originalNote(status))
    }
}
