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
    /// partial one.
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
        var seen: [Bool] = []
        try await waitUntil("the saved reading") { model.generation != generation }
        XCTAssertTrue(model.progress.complete, "rows and columns can be edited at once")
        while model.diagnostics?.complete != true {
            if let report = model.diagnostics { seen.append(report.complete) }
            try await Task.sleep(for: .milliseconds(1))
        }
        XCTAssertFalse(seen.contains(false), "no partial report taken")
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
        set(opened.model, 10, 1, "needle too")
        content.search(for: "needle")
        try await waitUntil("found again") { content.find.progress?.complete == true }
        XCTAssertEqual(content.find.matchCount, 2)
        XCTAssertNil(old, "the old reading's search is let go")
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
    /// isn't a change elsewhere: the next save asks nothing.
    func testTheSavedFilesReviewRecordsTheDelimiter() async throws {
        var text = "name,\n"
        while text.utf8.count < 64 * 1024 + 10 { text += "x\n" }
        for i in 0..<100_000 { text += "\(i);a;b\n" }
        let url = try file("later.csv", text)
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

        let first = opened.document.remembering
        set(model, 2, 0, "z")
        let saved = try await save(opened)
        XCTAssertTrue(saved)
        try await waitUntil("remembering again") { opened.document.remembering != first }
        let again = await opened.document.remembering?.value
        XCTAssertEqual(again, true)
        let resaved = try Data(contentsOf: url)
        XCTAssertEqual(attribute(url, Self.interpretation), "v=1;delimiter=comma;header=\(header);file=\(fingerprint(resaved))")
        await model.checkOriginal()?.value
        opened.content.updateBanners()
        XCTAssertNil(opened.content.driveBanner)
        XCTAssertTrue(alerts.isEmpty, "no prompt: \(alerts.map(\.messageText))")
    }
}
