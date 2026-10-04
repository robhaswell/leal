import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.5.3b against the real core, hosted (and sandboxed) in Leal.app:
/// after a save the window shows the reading the core made of the file it
/// wrote (ADR-0008 decision 1): no tile, strip or edited-cell mark of the
/// old reading is left; the diagnostics are the new reading's, taken only
/// once complete; Find searches the new reading; the column widths and
/// undo stay. Leal's own save is never a change elsewhere. A kept old file
/// is moved to the Recovered folder and reported. The attributes on disk
/// are the saved file's (ADR-0008 decision 8), including the delimiter a
/// whole-file review would suggest otherwise.
///
/// No sheet is shown (`CSVDocument.showSheet` is replaced) and nothing is
/// sent to the system (CLAUDE.md).
@MainActor
final class AfterSaveTests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?
    private var savedRecovered: (() throws -> URL)?
    private var savedDelay: Duration = .zero

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-after-save-\(UUID().uuidString)")
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
        savedDelay = DocumentModel.sizingAfterEditDelay
        DocumentModel.sizingAfterEditDelay = .milliseconds(1)
        savedRecovered = RecoveredFiles.folder
        let recovered = directory.appending(path: "Recovered")
        RecoveredFiles.folder = { recovered }
    }

    override func tearDown() async throws {
        debugReleaseHeldSave()
        CSVDocument.whileRememberingForTesting = nil
        CSVDocument.afterRememberingForTesting = nil
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        if let savedRecovered { RecoveredFiles.folder = savedRecovered }
        DocumentModel.sizingAfterEditDelay = savedDelay
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private struct Opened {
        let document: CSVDocument
        let model: DocumentModel
        let content: DocumentViewController
        let window: NSWindow
    }

    /// The sheets shown, in order; each is answered with its first button.
    private var alerts: [NSAlert] = []

    private func file(_ name: String, _ text: String) throws -> URL {
        let url = directory.appending(path: name)
        try Data(text.utf8).write(to: url)
        return url
    }

    /// Opens `url` with a window drawn through its strips, indexed.
    private func open(_ url: URL) async throws -> Opened {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        window.colorSpace = .sRGB
        window.appearance = NSAppearance(named: .aqua)
        let content = controller.content
        content.grid.stripScaleForTesting = 2
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        // The window is never shown in tests, so it counts as on screen.
        document.isOnScreen = { _ in true }
        document.showSheet = { [weak self] alert, _, done in
            self?.alerts.append(alert)
            done(.alertFirstButtonReturn)
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

    private func set(_ model: DocumentModel, _ row: Int, _ column: Int, _ value: String) {
        guard case .edited = model.setCell(.cell(CellPosition(row: row, column: column)), to: value) else {
            return XCTFail("not edited: \(row), \(column)")
        }
    }

    private func value(_ model: DocumentModel, _ row: Int, _ column: Int) -> String? {
        model.fullValue(.cell(CellPosition(row: row, column: column)))
    }

    /// Saves as ⌘S does, and waits for it; whether it saved.
    @discardableResult
    private func save(_ opened: Opened) async throws -> Bool {
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        return await saving.value
    }

    /// The grid as its strips hold it, and as drawn now from the model
    /// (as in `GridStripsTests`): equal when no strip is stale.
    private func strips(_ grid: GridContainerView) -> (held: Data, now: Data) {
        let view = grid.gridView
        let rect = view.visibleRect
        func bitmap() -> NSBitmapImageRep {
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(rect.width * 2), pixelsHigh: Int(rect.height * 2),
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
            )!.retagging(with: .sRGB)!
            rep.size = rect.size
            return rep
        }
        let held = bitmap()
        view.cacheDisplay(in: rect, to: held)
        let now = bitmap()
        let graphics = NSGraphicsContext(bitmapImageRep: now)!
        let context = graphics.cgContext
        context.concatenate(context.ctm.inverted())
        context.scaleBy(x: 2, y: 2)
        context.translateBy(x: 0, y: rect.height)
        context.scaleBy(x: 1, y: -1)
        context.translateBy(x: -rect.minX, y: -rect.minY)
        view.drawDirectly(rect, context: context)
        graphics.flushGraphics()
        func data(_ rep: NSBitmapImageRep) -> Data { Data(bytes: rep.bitmapData!, count: rep.bytesPerRow * rep.pixelsHigh) }
        return (data(held), data(now))
    }

    /// The extended attribute `name` of `url`, as text.
    private func attribute(_ url: URL, _ name: String) -> String? {
        let path = url.path(percentEncoded: false)
        let size = getxattr(path, name, nil, 0, 0, 0)
        guard size >= 0 else { return nil }
        var bytes = [UInt8](repeating: 0, count: size)
        guard getxattr(path, name, &bytes, size, 0, 0) == size else { return nil }
        return String(decoding: bytes, as: UTF8.self)
    }

    private func setAttribute(_ url: URL, _ name: String, _ value: String) {
        let bytes = Array(value.utf8)
        XCTAssertEqual(setxattr(url.path(percentEncoded: false), name, bytes, bytes.count, 0, 0), 0)
    }

    /// `Fingerprint::of` (ADR-0007): the length, and the FNV-1a hash of the
    /// first 64 KB.
    private func fingerprint(_ bytes: Data) -> String {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in bytes.prefix(64 * 1024) {
            hash = (hash ^ UInt64(byte)) &* 0x0100_0000_01b3
        }
        let hex = String(hash, radix: 16)
        return "\(bytes.count)-\(String(repeating: "0", count: 16 - hex.count))\(hex)"
    }

    private static let interpretation = "io.github.robhaswell.leal.interpretation"
    private static let textEncoding = "com.apple.TextEncoding"

    // MARK: The saved reading

    /// After a save the model reads the file it wrote, as a new reading:
    /// every strip is drawn from it (none holds the old reading's cells or
    /// marks), the saved edits lose their triangles while an edit made
    /// during the save keeps its own, the values are the saved ones, the
    /// widths and undo stay, and the diagnostics are the new reading's.
    func testTheGridShowsTheSavedReadingWithNoStaleTilesStripsOrMarks() async throws {
        var text = "id,name,city\n"
        for row in 0..<300 { text += row == 5 ? "5,short\n" : "\(row),n\(row),c\(row)\n" }
        let url = try file("reading.csv", text)
        let opened = try await open(url)
        let model = opened.model
        let grid = opened.content.grid
        try await waitUntil("diagnostics") { model.diagnostics?.complete == true && model.isSizingRefined }
        set(model, 0, 1, "saved")
        set(model, 1, 1, "also saved")
        opened.content.view.layoutSubtreeIfNeeded()
        let before = strips(grid)
        XCTAssertTrue(model.isEdited(row: 0, column: 1))
        XCTAssertEqual(before.held, before.now)
        let generation = model.generation

        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        set(model, 2, 1, "during")
        try await waitUntil("measured after the edits") { !model.isMeasuringAfterEdit }
        let widths = model.columnWidths
        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)

        XCTAssertNotEqual(model.generation, generation, "the saved file's reading")
        XCTAssertTrue(model.readingFromSave)
        XCTAssertEqual(value(model, 0, 1), "saved")
        XCTAssertEqual(value(model, 1, 1), "also saved")
        XCTAssertEqual(value(model, 2, 1), "during")
        opened.content.view.layoutSubtreeIfNeeded()
        let after = strips(grid)
        XCTAssertFalse(model.isEdited(row: 0, column: 1), "a saved edit has no triangle")
        XCTAssertFalse(model.isEdited(row: 1, column: 1))
        XCTAssertTrue(model.isEdited(row: 2, column: 1), "the edit made during the save is unsaved")
        XCTAssertEqual(after.held, after.now, "the strips hold the saved reading")
        XCTAssertNotEqual(after.held, before.held, "the triangles went")
        try await waitUntil("settled") { !model.isMeasuringAfterEdit && model.isSizingRefined }
        XCTAssertEqual(model.columnWidths, widths, "the widths stay")
        XCTAssertTrue(opened.document.history.undoManager.canUndo)
        try await waitUntil("the new diagnostics") { model.diagnostics?.complete == true }
        XCTAssertEqual(model.diagnostics?.generation, model.generation)
        XCTAssertTrue(model.rowHasMarker(5), "the short row's marker, from the new reading")
        opened.content.updateBanners()
        XCTAssertNotNil(opened.content.diagnosticsBanner)
        opened.document.history.undoManager.undo()
        XCTAssertEqual(value(model, 2, 1), "n2", "undo carries on")
        XCTAssertTrue(alerts.isEmpty)
    }

    /// After a save the index says it is complete at once (task 2.4c), but
    /// the diagnostics are taken only once their own report is: never a
    /// partial one. Until then the old reading's report stays shown, so
    /// the banner and the badge don't flicker.
    func testTheDiagnosticsWaitForTheSavedReadingsCompleteReport() async throws {
        var text = "id,name\n"
        for row in 0..<200_000 { text += "\(row),n\(row)\n" }
        text += "ragged,row,here\n"
        let url = try file("big.csv", text)
        let opened = try await open(url)
        let model = opened.model
        try await waitUntil("diagnostics") { model.diagnostics?.complete == true }
        set(model, 0, 1, "x")
        let generation = model.generation
        let saving = Task { try await self.save(opened) }
        // Whether each report of the new reading seen was complete, and
        // how often there was none at all.
        var seen: [Bool] = []
        var none = 0
        try await waitUntil("the saved reading") { model.generation != generation }
        XCTAssertTrue(model.progress.complete, "rows and columns can be edited at once")
        while model.diagnostics?.generation != model.generation || model.diagnostics?.complete != true {
            if let report = model.diagnostics {
                if report.generation == model.generation { seen.append(report.complete) }
            } else {
                none += 1
            }
            try await Task.sleep(for: .milliseconds(1))
        }
        XCTAssertFalse(seen.contains(false), "no partial report taken")
        XCTAssertEqual(none, 0, "the old report stays until the new one is complete")
        XCTAssertEqual(model.diagnostics?.generation, model.generation)
        XCTAssertEqual(model.diagnostics?.bannerKinds, 1)
        _ = try await saving.value
    }

    /// Find runs again on the saved reading: a new search, which finds the
    /// values as saved. Its old search is let go.
    func testFindRestartsAndFindsInTheSavedReading() async throws {
        var text = "id,name\n"
        for row in 0..<50 { text += "\(row),n\(row)\n" }
        let url = try file("find.csv", text)
        let opened = try await open(url)
        let content = opened.content
        set(opened.model, 3, 1, "needle")
        content.showFindBar()
        content.findBar.field.stringValue = "needle"
        content.search(for: "needle")
        try await waitUntil("found") { content.find.progress?.complete == true }
        XCTAssertEqual(content.find.matchCount, 1)
        weak let old = content.find.search
        XCTAssertNotNil(old)

        try await save(opened)

        try await waitUntil("searched again") { content.find.search !== old && content.find.progress?.complete == true }
        XCTAssertEqual(content.find.matchCount, 1)
        XCTAssertEqual(content.find.current?.cell, CellPosition(row: 3, column: 1), "still the current match")
        set(opened.model, 10, 1, "needle too")
        content.search(for: "needle")
        try await waitUntil("found again") { content.find.progress?.complete == true }
        XCTAssertEqual(content.find.matchCount, 2)
        XCTAssertNil(old, "the old reading's search is let go")
    }

    /// Find searches the saved reading again without stepping: a range
    /// selected whose active cell isn't a match stays as it is, and
    /// nothing is the current match.
    func testFindAfterASaveLeavesTheSelectionAlone() async throws {
        var text = "id,name\n"
        for row in 0..<50 { text += "\(row),n\(row)\n" }
        let url = try file("find-range.csv", text)
        let opened = try await open(url)
        let content = opened.content
        set(opened.model, 3, 1, "needle")
        content.showFindBar()
        content.findBar.field.stringValue = "needle"
        content.search(for: "needle")
        try await waitUntil("found") { content.find.progress?.complete == true }
        content.grid.select(CellPosition(row: 10, column: 0))
        content.grid.gridView.onExtend?(CellPosition(row: 14, column: 1))
        let selection = content.grid.selection
        XCTAssertEqual(selection?.rows, 10...14)
        XCTAssertNil(content.find.current)
        weak let old = content.find.search

        try await save(opened)

        try await waitUntil("searched again") { content.find.search !== old && content.find.progress?.complete == true }
        XCTAssertEqual(content.find.matchCount, 1)
        XCTAssertEqual(content.grid.selection, selection, "the range stays")
        XCTAssertEqual(content.grid.activeCell, CellPosition(row: 10, column: 0))
        XCTAssertNil(content.find.current)
    }

    /// A look at the file that ends after the save has ended but before its
    /// outcome reaches the model leaves the new reading to `saved(_:)`,
    /// which adopts it as a save's (not as a re-read).
    func testALookAtTheFileBeforeTheOutcomeArrivesLeavesTheSavedReadingToIt() async throws {
        let url = try file("gap.csv", "a,b\n1,2\n3,4\n")
        let opened = try await open(url)
        let model = opened.model
        set(model, 1, 1, "x")
        let generation = model.generation
        let place = await DocumentModel.placeForSaving(url)
        let job = try XCTUnwrap(model.startSave(to: url, kind: .save, place: place, overwriteChanged: false).get())
        let result = await DocumentModel.outcome(of: job)
        model.saveEnded(job, place: place, outcomeFollows: true)
        let saved = try result.get()
        // In the gap.
        await model.checkOriginal()?.value
        XCTAssertEqual(model.generation, generation, "left to saved(_:)")
        XCTAssertFalse(model.readingFromSave)

        model.saved(saved.outcome)
        XCTAssertNotEqual(model.generation, generation)
        XCTAssertTrue(model.readingFromSave, "adopted as the save's reading")
        XCTAssertFalse(model.saveOutcomePending)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), "a,b\n1,2\n3,x\n")
        XCTAssertTrue(alerts.isEmpty)
    }

    /// Leal's own save is never a change elsewhere (ADR-0008 decision 1):
    /// saved twice in a row, looked at again in between, with no banner
    /// and no prompt. A banner the user closed stays closed after a save.
    func testSavingTwiceInARowShowsNoBannerAndNoPrompt() async throws {
        var text = "id,name\n"
        for row in 0..<50 { text += row == 7 ? "7\n" : "\(row),n\(row)\n" }
        let url = try file("twice.csv", text)
        let opened = try await open(url)
        let model = opened.model
        let content = opened.content
        try await waitUntil("diagnostics") { model.diagnostics?.complete == true }
        content.updateBanners()
        let banner = try XCTUnwrap(content.diagnosticsBanner)
        banner.dismiss(nil)
        XCTAssertNil(content.diagnosticsBanner)

        for edit in ["first", "second"] {
            set(model, 0, 1, edit)
            let saved = try await save(opened)
            XCTAssertTrue(saved)
            await model.checkOriginal()?.value
            try await waitUntil("the new diagnostics") { model.diagnostics?.complete == true }
            content.updateBanners()
            XCTAssertNil(content.driveBanner, "no changed-elsewhere banner")
            XCTAssertNil(content.diagnosticsBanner, "the closed banner stays closed")
            XCTAssertFalse(model.original.diverged)
            XCTAssertEqual(model.original.state, .unchanged)
            XCTAssertFalse(opened.document.isDocumentEdited)
        }
        XCTAssertTrue(alerts.isEmpty, "no prompt: \(alerts.map(\.messageText))")
        XCTAssertNil(opened.window.attachedSheet)
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).hasPrefix("id,name\n0,second\n"))
    }

    // MARK: A kept old file

    /// A kept old file is moved at once to the Recovered folder, under its
    /// own name (or numbered if that is taken), its temporary folder is
    /// removed, and the user is told where it is. One that can't be moved
    /// is reported where it is.
    func testAKeptOldFileIsMovedToRecoveredAndReported() async throws {
        let url = try file("orders.csv", "a,b\n1,2\n")
        let opened = try await open(url)
        let recovered = directory.appending(path: "Recovered")
        func kept(_ text: String) throws -> String {
            let folder = directory.appending(path: "save-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            let kept = folder.appending(path: "orders (old).csv")
            try Data(text.utf8).write(to: kept)
            return kept.path(percentEncoded: false)
        }

        let first = try kept("another app's\n")
        let movedTo = await opened.document.keepOldFile(first)
        let moved = try XCTUnwrap(movedTo)
        XCTAssertEqual(moved.standardizedFileURL, recovered.appending(path: "orders (old).csv").standardizedFileURL)
        XCTAssertEqual(try String(contentsOf: moved, encoding: .utf8), "another app's\n")
        XCTAssertFalse(FileManager.default.fileExists(atPath: first))
        XCTAssertFalse(FileManager.default.fileExists(atPath: URL(filePath: first).deletingLastPathComponent().path(percentEncoded: false)), "its temporary folder goes")
        let alert = try XCTUnwrap(alerts.last)
        XCTAssertEqual(alert.messageText, "Leal saved “orders.csv” and kept the version it replaced.")
        XCTAssertEqual(alert.informativeText, "Another app may have changed the file as Leal saved it. Its version is in Leal’s Recovered folder, as “orders (old).csv”.")
        XCTAssertEqual(alert.buttons.map(\.title), ["OK", "Show in Finder"])

        let again = try kept("again\n")
        let movedAgain = await opened.document.keepOldFile(again)
        let second = try XCTUnwrap(movedAgain)
        XCTAssertEqual(second.lastPathComponent, "orders (old) 2.csv")
        XCTAssertEqual(try String(contentsOf: moved, encoding: .utf8), "another app's\n", "the first is kept too")

        let gone = directory.appending(path: "nowhere/orders (old).csv").path(percentEncoded: false)
        let notMoved = await opened.document.keepOldFile(gone)
        XCTAssertNil(notMoved)
        XCTAssertEqual(alerts.count, 3)
        XCTAssertTrue(alerts.last?.informativeText.contains(gone) == true, alerts.last?.informativeText ?? "")
    }

    /// The core's first choice is to keep the old file next to the user's
    /// (`… (replaced, kept by Leal).csv`): it stays there, and the user is
    /// told so.
    func testAKeptOldFileNextToTheUsersFileStaysThere() async throws {
        let url = try file("orders.csv", "a,b\n1,2\n")
        let opened = try await open(url)
        let beside = try file("orders (replaced, kept by Leal).csv", "another app's\n")
        let kept = await opened.document.keepOldFile(beside.path(percentEncoded: false))
        XCTAssertEqual(kept?.standardizedFileURL, beside.standardizedFileURL)
        XCTAssertEqual(try String(contentsOf: beside, encoding: .utf8), "another app's\n")
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appending(path: "Recovered").path(percentEncoded: false)), "nothing moved")
        let alert = try XCTUnwrap(alerts.last)
        XCTAssertEqual(alert.messageText, "Leal saved “orders.csv” and kept the version it replaced.")
        XCTAssertEqual(alert.informativeText, "Another app may have changed the file as Leal saved it. Its version is next to it, as “orders (replaced, kept by Leal).csv”.")
        XCTAssertEqual(alert.buttons.map(\.title), ["OK", "Show in Finder"])
    }

    /// With no window (Save and Close, or ⌘S then ⌘Q, closed it), the
    /// user is told all the same, in an app-modal alert.
    func testAKeptOldFileIsReportedWithNoWindow() async throws {
        let url = try file("closed.csv", "a,b\n1,2\n")
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        XCTAssertTrue(document.windowControllers.isEmpty)
        document.showSheet = { _, _, done in
            XCTFail("no window for a sheet")
            done(.alertFirstButtonReturn)
        }
        document.showAlert = { [weak self] alert, done in
            self?.alerts.append(alert)
            done(.alertFirstButtonReturn)
        }
        let folder = directory.appending(path: "save-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let path = folder.appending(path: "closed (old).csv")
        try Data("old\n".utf8).write(to: path)
        let moved = await document.keepOldFile(path.path(percentEncoded: false))
        XCTAssertEqual(moved?.lastPathComponent, "closed (old).csv")
        XCTAssertEqual(alerts.count, 1)
        XCTAssertEqual(alerts.first?.messageText, "Leal saved “closed.csv” and kept the version it replaced.")
        XCTAssertEqual(alerts.first?.informativeText, "Another app may have changed the file as Leal saved it. Its version is in Leal’s Recovered folder, as “closed (old).csv”.")
        document.close()
    }

    /// A window nobody can see (minimised, or ordered out as when the app
    /// is hidden) gets no sheet, which would never be answered and would
    /// hang the save, a close or a quit: the alert is app-modal instead.
    func testAKeptOldFileIsReportedAppModallyWhileTheWindowIsOutOfSight() async throws {
        let opened = try await open(try file("hidden.csv", "a,b\n1,2\n"))
        var sheets = 0
        opened.document.showSheet = { _, _, _ in sheets += 1 }
        var modal = 0
        opened.document.showAlert = { _, done in
            modal += 1
            done(.alertFirstButtonReturn)
        }
        func keep(_ name: String) async throws -> URL? {
            let folder = directory.appending(path: "save-\(UUID().uuidString)")
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            let path = folder.appending(path: name)
            try Data("old\n".utf8).write(to: path)
            // A sheet that is never answered fails by the timeout, not by
            // hanging the run.
            let document = opened.document
            let keeping = Task { await document.keepOldFile(path.path(percentEncoded: false)) }
            var finished = false
            Task {
                _ = await keeping.value
                finished = true
            }
            try await waitUntil("the kept file was reported", timeout: 10) { finished }
            return finished ? await keeping.value : nil
        }

        // Minimised, as the real check sees it.
        opened.document.isOnScreen = { _ in false }
        let first = try await keep("hidden (old).csv")
        XCTAssertNotNil(first)
        XCTAssertEqual(modal, 1)
        XCTAssertEqual(sheets, 0, "nobody would see a sheet")

        // Ordered out, with the document's own check of the window.
        opened.document.isOnScreen = { $0.isVisible && !$0.isMiniaturized }
        opened.window.orderOut(nil)
        XCTAssertFalse(opened.window.isVisible)
        let second = try await keep("hidden (old) 2.csv")
        XCTAssertNotNil(second)
        XCTAssertEqual(modal, 2)
        XCTAssertEqual(sheets, 0)

        // Back on screen, the sheet is used.
        opened.document.isOnScreen = { _ in true }
        opened.document.showSheet = { _, _, done in
            sheets += 1
            done(.alertFirstButtonReturn)
        }
        let third = try await keep("hidden (old) 3.csv")
        XCTAssertNotNil(third)
        XCTAssertEqual(sheets, 1)
        XCTAssertEqual(modal, 2)
    }

    // MARK: Attributes (ADR-0008 decision 8)

    /// After Save the attributes on disk are the saved file's own, not
    /// ones copied from the old file: `com.apple.TextEncoding` (the file
    /// had one) and the interpretation attribute, which records the
    /// remembered choices with the new bytes' fingerprint. An unreadable
    /// interpretation attribute is removed.
    func testTheAttributesOnDiskAreTheSavedFilesOwn() async throws {
        let url = try file("remembered.csv", "a;b\n1;2\n3;4\n")
        setAttribute(url, Self.interpretation, "v=1;delimiter=semicolon;header=no;file=1-0000000000000000")
        setAttribute(url, Self.textEncoding, "utf-8;134217984")
        let opened = try await open(url)
        XCTAssertEqual(opened.model.interpretation.delimiter, .semicolon)
        XCTAssertFalse(opened.model.interpretation.header)
        set(opened.model, 1, 1, "22")
        try await save(opened)
        let bytes = try Data(contentsOf: url)
        XCTAssertEqual(String(decoding: bytes, as: UTF8.self), "a;b\n1;22\n3;4\n")
        XCTAssertEqual(attribute(url, Self.interpretation), "v=1;delimiter=semicolon;header=no;file=\(fingerprint(bytes))")
        XCTAssertEqual(attribute(url, Self.textEncoding), "utf-8;134217984")

        let garbled = try file("garbled.csv", "a,b\n1,2\n")
        setAttribute(garbled, Self.interpretation, "v=9;junk")
        let other = try await open(garbled)
        set(other.model, 0, 1, "3")
        try await save(other)
        XCTAssertNil(attribute(garbled, Self.interpretation), "removed, not copied over")
        XCTAssertTrue(alerts.isEmpty)
    }

    /// The whole-file part of ADR-0008 decision 8: the first 64 KB read
    /// comma-separated, the whole file looks semicolon-separated. The save
    /// can't tell; the review of its reading can, and the delimiter is
    /// recorded then, with the saved bytes' fingerprint. The attribute
    /// isn't a change elsewhere: the next save asks nothing, and records
    /// the delimiter itself ("on every save", ADR-0008 decision 8), so it
    /// is there even if the window closes at once.
    func testTheSavedFilesReviewRecordsTheDelimiter() async throws {
        let url = try file("later.csv", Self.laterDelimiter)
        let opened = try await open(url)
        let model = opened.model
        try await waitUntil("reviewed") { model.review != nil }
        XCTAssertEqual(model.interpretation.delimiter, .comma)
        XCTAssertEqual(model.review?.delimiterSuggestion, .semicolon)
        XCTAssertNil(opened.document.remembering, "not after an open")

        set(model, 1, 0, "y")
        try await save(opened)
        XCTAssertNil(attribute(url, Self.interpretation), "the save itself can't tell")
        try await waitUntil("remembering") { opened.document.remembering != nil }
        let wrote = await opened.document.remembering?.value
        XCTAssertEqual(wrote, true)
        let bytes = try Data(contentsOf: url)
        let header = model.interpretation.header ? "yes" : "no"
        XCTAssertEqual(attribute(url, Self.interpretation), "v=1;delimiter=comma;header=\(header);file=\(fingerprint(bytes))")

        await model.checkOriginal()?.value
        opened.content.updateBanners()
        XCTAssertNil(opened.content.driveBanner)
        set(model, 2, 0, "z")
        let saved = try await save(opened)
        XCTAssertTrue(saved)
        // Closed at once: no review of this save's reading is waited for.
        opened.document.close()
        let resaved = try Data(contentsOf: url)
        XCTAssertEqual(attribute(url, Self.interpretation), "v=1;delimiter=comma;header=\(header);file=\(fingerprint(resaved))")
        XCTAssertTrue(alerts.isEmpty, "no prompt: \(alerts.map(\.messageText))")
    }

    /// Comma-separated in its first 64 KB, semicolon-separated after: the
    /// whole-file review suggests semicolons.
    private static let laterDelimiter: String = {
        var text = "name,\n"
        while text.utf8.count < 64 * 1024 + 10 { text += "x\n" }
        for i in 0..<100_000 { text += "\(i);a;b\n" }
        return text
    }()

    /// Recording the delimiter holds a coordinated write of the file, which
    /// other apps' coordinated access waits for. Meanwhile the main thread
    /// may be blocked: `NSDocument` blocks it wherever it reads
    /// `isDocumentEdited` while a save holds the document's file access. So
    /// the recording must end its write without the main thread. Here the
    /// recording is held, a save starts, the main thread reads
    /// `isDocumentEdited` and then waits for the write to end, and a
    /// background thread lets the recording go. A deadlock would hang the
    /// main thread for good: the watchdog then stops the test host, so it
    /// fails rather than hangs.
    func testRecordingTheDelimiterNeverDeadlocksWithASave() async throws {
        let url = try file("held.csv", Self.laterDelimiter)
        let opened = try await open(url)
        let model = opened.model
        let hold = Hold()
        let ended = DispatchSemaphore(value: 0)
        CSVDocument.whileRememberingForTesting = { hold.hold() }
        CSVDocument.afterRememberingForTesting = { ended.signal() }
        set(model, 1, 0, "y")
        try await save(opened)
        try await waitUntil("the recording held") { hold.isHolding }

        set(model, 2, 0, "z")
        let watchdog = Watchdog(seconds: 30, what: "the main thread, waiting while the delimiter is recorded")
        // Let go from another thread, whatever the main thread waits for.
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { hold.release() }
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        // Synchronously, on the main thread: as NSDocument does while the
        // save holds its file access, then until the recording's write ends.
        _ = opened.document.isDocumentEdited
        _ = opened.document.fileModificationDate
        blockMainThread(until: ended)
        watchdog.stop()
        XCTAssertTrue(hold.isReleased)

        let saved = await saving.value
        XCTAssertTrue(saved)
        _ = await opened.document.remembering?.value
        XCTAssertFalse(opened.document.isDocumentEdited)
        let header = model.interpretation.header ? "yes" : "no"
        // Recorded by the review or by the save, whichever came last.
        try await waitUntil("the delimiter recorded") {
            guard let bytes = try? Data(contentsOf: url) else { return false }
            return attribute(url, Self.interpretation) == "v=1;delimiter=comma;header=\(header);file=\(fingerprint(bytes))"
        }
        let text = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(text.contains("\ny\n") && text.contains("\nz\n"), "both edits saved")
        XCTAssertTrue(alerts.isEmpty, "no prompt: \(alerts.map(\.messageText))")
    }
}

/// Blocks the calling thread (the main thread) until `semaphore` is
/// signalled, as `NSDocument` blocks it waiting for a file access.
private func blockMainThread(until semaphore: DispatchSemaphore) {
    semaphore.wait()
}

/// Holds the first caller of `hold()` until `release()`; later callers go
/// on at once.
private final class Hold: @unchecked Sendable {
    // @unchecked: `holding` and `released` are only touched under `lock`.
    private let lock = NSLock()
    private var holding = false
    private var released = false
    private let go = DispatchSemaphore(value: 0)

    var isHolding: Bool { lock.withLock { holding } }
    var isReleased: Bool { lock.withLock { released } }

    func hold() {
        let wait = lock.withLock { () -> Bool in
            guard !released, !holding else { return false }
            holding = true
            return true
        }
        if wait { go.wait() }
    }

    func release() {
        lock.withLock { released = true }
        go.signal()
    }
}
