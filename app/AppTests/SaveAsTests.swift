import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.5.3c against the real core, hosted (and sandboxed) in Leal.app:
/// Save As goes through the core inside a coordinated write, and the
/// document then is the new file (its URL, title, model, watcher and Open
/// Recent); over an existing file too. From a document Leal couldn't read
/// all of (each file banner's Save As…) it writes complete rows only and
/// says so. Duplicate saves a copy named "copy". Revert to Saved asks,
/// then reads the file again off the main thread. A save that ends with
/// `SaveJob.restarted()` takes up the new reading; a drive coming back
/// keeps the edits. The main thread never waits for a Save As.
///
/// No save panel or sheet is shown (`chooseSaveAsDestination` and
/// `showSheet` are replaced) and nothing is sent to the system (CLAUDE.md).
@MainActor
final class SaveAsTests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?
    private var savedNoteRecent: ((URL) -> Void)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-save-as-\(UUID().uuidString)")
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
        savedNoteRecent = CSVDocument.noteRecentDocument
        CSVDocument.noteRecentDocument = { [weak self] url in
            MainActor.assumeIsolated { self?.recent.append(url) }
        }
    }

    override func tearDown() async throws {
        debugReleaseHeldSave()
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        if let savedNoteRecent { CSVDocument.noteRecentDocument = savedNoteRecent }
        DocumentModel.openForTesting = nil
        let shared = NSDocumentController.shared.documents
        for document in opened where !shared.contains(document) { document.close() }
        opened.removeAll()
        for document in shared {
            document.close()
        }
        let files = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        for url in files { _ = CSVDocument.unlock(url) }
        CoreRelease.finish()
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private let csv = "id,name,qty\r\n1,Marlow,3\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n"

    /// The documents opened, kept alive for the whole test.
    private var opened: [CSVDocument] = []
    /// The sheets shown, in order.
    private var alerts: [NSAlert] = []
    /// The buttons to answer the next sheets with, by index (the last, the
    /// way out, by default).
    private var answers: [Int] = []
    /// What the save panel was asked, in order.
    private var panels: [CSVDocument.SaveAsRequest] = []
    /// Where the save panel answers (`nil`: cancelled).
    private var destination: URL?
    /// What went in Open Recent.
    private var recent: [URL] = []

    private func file(_ name: String, _ text: String) throws -> URL {
        let url = directory.appending(path: name)
        try Data(text.utf8).write(to: url)
        return url
    }

    /// `rows` data rows after a header, each its number and `prefix` with it.
    private func text(rows: Int, prefix: String = "name") -> String {
        var text = "id,name\n"
        for i in 0..<rows { text += "\(i),\(prefix) \(i)\n" }
        return text
    }

    /// Hooks a document's panel, sheets and window up to the test.
    private func attach(_ document: CSVDocument) throws -> (DocumentModel, DocumentViewController) {
        if document.windowControllers.isEmpty { document.makeWindowControllers() }
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        controller.content.view.layoutSubtreeIfNeeded()
        document.isOnScreen = { _ in true }
        document.showSheet = { [weak self] alert, _, done in
            guard let self else { return }
            alerts.append(alert)
            let index = answers.isEmpty ? alert.buttons.count - 1 : answers.removeFirst()
            done(NSApplication.ModalResponse(rawValue: NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + index))
        }
        document.chooseSaveAsDestination = { [weak self] request, _, done in
            guard let self else { return done(nil) }
            panels.append(request)
            done(destination)
        }
        opened.append(document)
        return (try XCTUnwrap(document.model), controller.content)
    }

    /// Opens `url` with a window, as `CSVDocument(contentsOf:)` does, and
    /// waits for its index unless told not to.
    private func open(_ url: URL, indexed: Bool = true) async throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        let (model, content) = try attach(document)
        if indexed { try await waitUntil("indexed") { model.isIndexComplete } }
        return (document, model, content)
    }

    /// Opens `url` through `NSDocumentController`, which opens the core's
    /// document off the main thread (for a simulated share).
    private func openThroughController(_ url: URL) async throws -> (CSVDocument, DocumentModel, DocumentViewController) {
        let document: NSDocument = try await withCheckedThrowingContinuation { continuation in
            NSDocumentController.shared.openDocument(withContentsOf: url, display: false) { document, _, error in
                if let document {
                    continuation.resume(returning: document)
                } else {
                    continuation.resume(throwing: error ?? NSError(domain: "SaveAsTests", code: 1))
                }
            }
        }
        let csv = try XCTUnwrap(document as? CSVDocument)
        let (model, content) = try attach(csv)
        return (csv, model, content)
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

    /// The same file, however the paths are spelt.
    private func same(_ a: URL?, _ b: URL) -> Bool {
        a?.resolvingSymlinksInPath().standardizedFileURL.path(percentEncoded: false)
            == b.resolvingSymlinksInPath().standardizedFileURL.path(percentEncoded: false)
    }

    private func contents(_ url: URL) throws -> String {
        try String(contentsOf: url, encoding: .utf8)
    }

    private func modificationDate(_ url: URL) throws -> Date? {
        try FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))[.modificationDate] as? Date
    }

    /// Asks Save As as File ▸ Save As… does, and waits for it.
    @discardableResult
    private func saveAs(_ document: CSVDocument, to url: URL?) async throws -> Bool {
        destination = url
        document.saveAs(nil)
        guard url != nil else { return false }
        return try await XCTUnwrap(document.saving).value
    }

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

    /// Opens files through the core as usual, recording on which thread
    /// (`threads`) and waiting while `gate` is held: a Reload or Revert
    /// held part-way through reading the file.
    private func hookOpens(_ threads: OpenThreads, gate: OpenGate? = nil) {
        DocumentModel.openForTesting = { path, environment, options, observer in
            threads.record()
            gate?.passThrough()
            return try openDocument(
                path: path,
                volume: TemporaryFolders.volume(for: URL(filePath: path)),
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer
            )
        }
    }

    // MARK: Save As

    /// Save As writes what Save would (the edits, quoted as needed), leaves
    /// the old file alone, and the document is then the new file: its URL,
    /// title, type, model, Open Recent, modification date and dirty state.
    /// The watcher watches the new file: a change to it is seen, and one
    /// to the old file isn't. Undo carries on, and Save writes the new file.
    func testSaveAsWritesTheEditsAndTheDocumentFollowsTheNewFile() async throws {
        let url = try file("orders.csv", csv)
        let (document, model, content) = try await open(url)
        set(model, 0, 1, "Marlow, Ltd")
        set(model, 2, 2, "9")
        let copy = directory.appending(path: "orders 2025.tsv")
        let generation = model.generation

        let saved = try await saveAs(document, to: copy)

        XCTAssertTrue(saved)
        XCTAssertEqual(panels.map(\.name), ["orders.csv"])
        XCTAssertEqual(panels.map(\.folder.lastPathComponent), [directory.lastPathComponent])
        XCTAssertEqual(panels.map(\.message), [nil], "the copy is complete")
        XCTAssertEqual(try contents(copy), "id,name,qty\r\n1,\"Marlow, Ltd\",3\r\n2,\"Ostrava\",5\r\n3,Halden,9\r\n")
        XCTAssertEqual(try contents(url), csv, "the old file is untouched")
        XCTAssertTrue(same(document.fileURL, copy))
        XCTAssertEqual(document.displayName, "orders 2025.tsv")
        XCTAssertEqual(document.fileType, "public.tab-separated-values-text")
        XCTAssertEqual(content.view.window?.title, "orders 2025.tsv")
        XCTAssertTrue(same(model.url, copy))
        XCTAssertTrue(same(URL(filePath: model.original.path), copy))
        XCTAssertEqual(recent.count, 1); XCTAssertTrue(same(recent.first, copy))
        XCTAssertEqual(document.fileModificationDate, try modificationDate(copy))
        XCTAssertNotEqual(model.generation, generation, "the core's reading of the new file, adopted")
        XCTAssertTrue(model.readingFromSave)
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertTrue(document.history.journal.isEmpty)
        XCTAssertTrue(alerts.isEmpty, alerts.map(\.messageText).joined())

        // The watcher follows the new file.
        try Data("id,name,qty\r\n".utf8).write(to: url)
        await model.checkOriginal()?.value
        XCTAssertEqual(model.original.state, .unchanged, "the old file is no longer the document's")
        try Data("other\n".utf8).write(to: copy, options: .atomic)
        try await waitUntil("the change to the new file is seen") {
            if model.original.state == .changed { return true }
            model.checkOriginal()
            return false
        }
        // Back as Leal saved it, then undo and Save: the new file is written.
        XCTAssertTrue(document.history.undoManager.canUndo)
        document.history.undoManager.undo()
        XCTAssertEqual(value(model, 2, 2), "8")
        answers = [0] // Save Anyway: the copy changed elsewhere.
        document.save(nil)
        let resaved = try await XCTUnwrap(document.saving).value
        XCTAssertTrue(resaved)
        XCTAssertEqual(try contents(copy), "id,name,qty\r\n1,\"Marlow, Ltd\",3\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n")
        XCTAssertEqual(try contents(url), "id,name,qty\r\n")
    }

    /// Save As over a file that is there (the panel asked first) replaces
    /// it, keeping its metadata (an extended attribute, its permissions),
    /// and the document follows it. Cancelling the panel saves nothing.
    func testSaveAsOverAnExistingFileReplacesIt() async throws {
        let url = try file("people.csv", csv)
        let target = try file("archive.csv", "old,contents\n")
        let path = target.path(percentEncoded: false)
        XCTAssertEqual(setxattr(path, "com.example.leal-test", "kept", 4, 0, 0), 0)
        XCTAssertEqual(chmod(path, 0o640), 0)
        let (document, model, _) = try await open(url)
        set(model, 1, 1, "Ostrava East")

        let cancelled = try await saveAs(document, to: nil)
        XCTAssertFalse(cancelled, "the panel was cancelled")
        XCTAssertEqual(try contents(target), "old,contents\n")
        XCTAssertTrue(document.isDocumentEdited)

        let savedtarget = try await saveAs(document, to: target)
        XCTAssertTrue(savedtarget)
        XCTAssertEqual(try contents(target), "id,name,qty\r\n1,Marlow,3\r\n2,\"Ostrava East\",5\r\n3,Halden,8\r\n", "a quoted field stays quoted")
        var buffer = [UInt8](repeating: 0, count: 16)
        let length = getxattr(path, "com.example.leal-test", &buffer, buffer.count, 0, 0)
        XCTAssertEqual(length, 4)
        XCTAssertEqual(String(decoding: buffer.prefix(max(0, length)), as: UTF8.self), "kept")
        let mode = try XCTUnwrap(FileManager.default.attributesOfItem(atPath: path)[.posixPermissions] as? Int)
        XCTAssertEqual(mode, 0o640)
        XCTAssertEqual(try contents(url), csv)
        XCTAssertTrue(same(document.fileURL, target))
        XCTAssertTrue(same(model.url, target))
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertTrue(alerts.isEmpty, alerts.map(\.messageText).joined())
    }

    /// Save As to the document's own file is a Save, with Save's checks.
    func testSaveAsToTheSameFileIsASave() async throws {
        let url = try file("same.csv", csv)
        let (document, model, _) = try await open(url)
        set(model, 0, 1, "Marlowe")
        let savedurl = try await saveAs(document, to: url)
        XCTAssertTrue(savedurl)
        XCTAssertTrue(try contents(url).contains("1,Marlowe,3"))
        XCTAssertTrue(recent.isEmpty, "Save, not Save As")
        XCTAssertFalse(document.isDocumentEdited)
    }

    /// Save As to the document's own file by another name for it, a
    /// symbolic link or (on a volume that ignores case) another case, is a
    /// Save too: a change made elsewhere asks first, as Save does, and the
    /// file keeps its name (task 2.5.3c review).
    func testSaveAsToTheSameFileByAnotherNameIsASave() async throws {
        let url = try file("Same.csv", csv)
        let link = directory.appending(path: "link.csv")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: url)
        let otherCase = directory.appending(path: "SAME.CSV")
        let caseInsensitive = FileManager.default.fileExists(atPath: otherCase.path(percentEncoded: false))
        let (document, model, _) = try await open(url)
        for (index, target) in ([link] + (caseInsensitive ? [otherCase] : [])).enumerated() {
            set(model, 0, 1, "Marlowe \(index)")
            let saved = try await saveAs(document, to: target)
            XCTAssertTrue(saved)
            XCTAssertTrue(try contents(url).contains("1,Marlowe \(index),3"))
            XCTAssertTrue(recent.isEmpty, "Save, not Save As")
            XCTAssertEqual(document.fileURL?.lastPathComponent, "Same.csv")
            XCTAssertFalse(document.isDocumentEdited)
        }
        let names = try FileManager.default.contentsOfDirectory(atPath: directory.path(percentEncoded: false)).filter { $0.hasSuffix(".csv") }
        XCTAssertEqual(Set(names), ["Same.csv", "link.csv"])
        let attributes = try FileManager.default.attributesOfItem(atPath: link.path(percentEncoded: false))
        XCTAssertEqual(attributes[.type] as? FileAttributeType, .typeSymbolicLink, "the link stays a link")

        // Changed elsewhere: Save's question, not a silent Save As.
        set(model, 0, 1, "Mine")
        try Data((csv + "4,Other,1\r\n").utf8).write(to: url)
        answers = [1] // Cancel.
        let saved = try await saveAs(document, to: link)
        XCTAssertFalse(saved)
        XCTAssertEqual(alerts.last?.messageText, SaveText.changedElsewhere(name: "Same.csv").title)
        XCTAssertTrue(try contents(url).contains("4,Other,1"))
    }

    /// Save As where it can't write says so in the catalog's words, and
    /// leaves the document as it was.
    func testSaveAsOntoAFolderSaysWhy() async throws {
        let url = try file("folder.csv", csv)
        let folder = directory.appending(path: "taken.csv")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        let (document, model, _) = try await open(url)
        set(model, 0, 1, "Marlowe")
        let savedfolder = try await saveAs(document, to: folder)
        XCTAssertFalse(savedfolder)
        XCTAssertEqual(alerts.map(\.messageText), ["“taken.csv” wasn’t saved."])
        XCTAssertEqual(alerts.first?.informativeText, "Something other than a file is at that place.")
        XCTAssertTrue(same(document.fileURL, url))
        XCTAssertTrue(document.isDocumentEdited)
    }

    // MARK: Incomplete Save As (ADR-0008 decision 6, ADR-0010)

    /// The copy of a document Leal couldn't read all of: complete rows
    /// only, a prefix of the file with the edit, ending at a row's end.
    /// The panel and the alert after say so plainly ("about N of M rows").
    /// The window then edits the copy, which is complete in itself.
    private func checkIncompleteSaveAs(
        _ document: CSVDocument,
        _ model: DocumentModel,
        _ content: DocumentViewController,
        original: String,
        click: () -> Void
    ) async throws {
        let read = try XCTUnwrap(model.incompleteCopy)
        XCTAssertLessThanOrEqual(read.rows, read.of)
        let copy = directory.appending(path: "copy.csv")
        destination = copy
        click()
        let saved = try await XCTUnwrap(document.saving).value
        XCTAssertTrue(saved)

        let message = try XCTUnwrap(panels.last?.message)
        XCTAssertEqual(message, SaveText.incompleteCopyMessage(rows: read.rows, of: read.of))
        XCTAssertTrue(message.contains("this copy will be incomplete"), message)
        let written = try contents(copy)
        let expected = original.replacingOccurrences(of: "0,name 0\n", with: "0,edited\n")
        XCTAssertTrue(expected.hasPrefix(written), "a prefix of the file")
        XCTAssertTrue(written.hasSuffix("\n"), "complete rows only")
        let rows = written.split(separator: "\n").count - 1
        XCTAssertGreaterThan(rows, 0)
        XCTAssertLessThan(rows, 20_000)
        XCTAssertTrue(written.hasPrefix("id,name\n0,edited\n"))
        let alert = try XCTUnwrap(alerts.last)
        XCTAssertEqual(alert.messageText, "The copy “copy.csv” is incomplete.")
        XCTAssertTrue(alert.informativeText.hasPrefix("It has "), alert.informativeText)
        XCTAssertTrue(alert.informativeText.contains(" \(rows.formatted()) "), alert.informativeText)

        XCTAssertTrue(same(document.fileURL, copy))
        try await waitUntil("the copy's reading") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, rows)
        XCTAssertNil(model.incompleteCopy)
        XCTAssertTrue(document.canSave)
        XCTAssertNil(content.driveBanner)
        XCTAssertFalse(document.isDocumentEdited)
    }

    /// The disconnected banner's Save As….
    func testTheDisconnectedBannersSaveAsWritesCompleteRowsOnly() async throws {
        simulateDrive(.disconnect(at: 100_000))
        let original = text(rows: 20_000)
        let url = try file("usb.csv", original)
        let (document, model, content) = try await open(url, indexed: false)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        set(model, 0, 1, "edited")
        try await waitUntil("its banner") { content.driveBanner != nil }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.disconnected)
        try await checkIncompleteSaveAs(document, model, content, original: original) {
            try? XCTUnwrap(banner.button).performClick(nil)
        }
    }

    /// The changed-while-reading banner's Save As… (its second button).
    func testTheChangedWhileReadingBannersSaveAsWritesCompleteRowsOnly() async throws {
        simulateDrive(.change(at: 16_384))
        let original = text(rows: 20_000)
        let url = try file("exfat.csv", original)
        let (document, model, content) = try await open(url, indexed: false)
        try await waitUntil("the change is seen") { model.changedOnDisk }
        set(model, 0, 1, "edited")
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, DiagnosticsText.changedWhileReading)
        try await checkIncompleteSaveAs(document, model, content, original: original) {
            try? XCTUnwrap(banner.secondaryButton).performClick(nil)
        }
    }

    /// The deleted-on-its-share banner's Save As….
    func testTheDeletedWhileReadingBannersSaveAsWritesCompleteRowsOnly() async throws {
        let original = text(rows: 20_000)
        let url = try file("share.csv", original)
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentSimulatingShare(
                path: path,
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                chunkBytes: 32_768,
                readDelayMs: 0,
                failure: SimulatedShareFailure(at: 100_000, errno: ESTALE, times: nil),
                holdAt: 100_000
            )
        }
        let (document, model, content) = try await openThroughController(url)
        try FileManager.default.removeItem(at: url)
        model.backgroundHandle()?.debugShareRelease()
        try await waitUntil("deleted") { model.storage == .deleted }
        try await waitUntil("the index stopped") { model.isIndexComplete }
        set(model, 0, 1, "edited")
        try await waitUntil("its banner") { content.driveBanner != nil }
        let banner = try XCTUnwrap(content.driveBanner)
        XCTAssertEqual(banner.message, FileBanner.deletedWhileReadingMessage)
        try await checkIncompleteSaveAs(document, model, content, original: original) {
            try? XCTUnwrap(banner.button).performClick(nil)
        }
    }

    /// Edits to rows an incomplete copy doesn't have are named.
    func testAnIncompleteCopyNamesTheEditsItCouldntKeep() {
        let refusal = SaveText.incompleteCopy(
            name: "copy.csv",
            rows: 1_200,
            of: 20_000,
            skipped: (0..<10).map { CellPlace(row: UInt64(5_000 + $0), column: 1) },
            headerRows: 1
        )
        XCTAssertEqual(refusal.title, "The copy “copy.csv” is incomplete.")
        let lines = refusal.detail.components(separatedBy: "\n")
        XCTAssertEqual(lines.first, "It has about 1,200 of 20,000 rows: only the rows Leal had read in full.")
        XCTAssertEqual(lines[2], "These edits weren’t saved, because their rows aren’t in the copy:")
        XCTAssertEqual(lines.count, 3 + 8 + 1)
        XCTAssertEqual(lines.last, "and 2 more")
        XCTAssertEqual(refusal.choices, [.ok])

        // With no total to give (the file went before Leal could tell).
        let unknown = SaveText.incompleteCopy(name: "copy.csv", rows: 6_700, of: 6_700, skipped: [], headerRows: 1)
        XCTAssertEqual(unknown.detail, "It has only the 6,700 rows Leal had read in full, not the whole file.")
        XCTAssertEqual(
            SaveText.incompleteCopyMessage(rows: 1_200, of: 20_000),
            "Leal has read only about 1,200 of 20,000 rows, so this copy will be incomplete: it gets only the rows Leal has read in full."
        )
        XCTAssertEqual(
            SaveText.incompleteCopyMessage(rows: 6_700, of: 6_700),
            "Leal couldn’t read all of the file, so this copy will be incomplete: it gets only the 6,700 rows Leal has read in full."
        )
    }

    // MARK: Duplicate

    /// Save's alert for a locked file offers Duplicate: a copy named
    /// "copy", with the edits, which the window then edits. The locked
    /// file is untouched and stays locked.
    func testDuplicateFromALockedFileSavesACopyAndEditsIt() async throws {
        let url = try file("locked.csv", csv)
        XCTAssertEqual(chflags(url.path(percentEncoded: false), UInt32(UF_IMMUTABLE)), 0)
        let (document, model, _) = try await open(url)
        set(model, 0, 1, "Marlowe")
        let copy = directory.appending(path: "locked copy.csv")
        destination = copy
        answers = [1] // Unlock, Duplicate, Cancel.
        document.save(nil)
        _ = try await XCTUnwrap(document.saving).value
        try await waitUntil("the duplicate is saved") { same(document.fileURL, copy) && document.saving == nil }

        XCTAssertEqual(alerts.first?.messageText, "“locked.csv” is locked.")
        XCTAssertEqual(panels.map(\.name), ["locked copy.csv"])
        XCTAssertEqual(try contents(copy), "id,name,qty\r\n1,Marlowe,3\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n")
        XCTAssertEqual(try contents(url), csv)
        var info = stat()
        XCTAssertEqual(stat(url.path(percentEncoded: false), &info), 0)
        XCTAssertNotEqual(info.st_flags & UInt32(UF_IMMUTABLE), 0, "still locked")
        XCTAssertTrue(same(model.url, copy))
        XCTAssertFalse(document.isDocumentEdited)
    }

    /// AppKit's Duplicate (`duplicateDocument:`) is the same, and so is
    /// any ask for `NSDocument`'s own save panel: never its writing.
    func testDuplicateAndAppKitsSavePanelGoThroughTheCore() async throws {
        let url = try file("plain.csv", csv)
        let (document, model, _) = try await open(url)
        set(model, 0, 1, "Marlowe")
        destination = directory.appending(path: "plain copy.csv")
        document.duplicate(nil)
        _ = try await XCTUnwrap(document.saving).value
        XCTAssertEqual(panels.map(\.name), ["plain copy.csv"])
        XCTAssertTrue(try contents(XCTUnwrap(destination)).contains("1,Marlowe,3"))

        set(model, 0, 1, "Marlow")
        let other = directory.appending(path: "panel.csv")
        destination = other
        let delegate = SaveAsDelegate()
        document.runModalSavePanel(
            for: .saveAsOperation,
            delegate: delegate,
            didSave: #selector(SaveAsDelegate.document(_:didSave:contextInfo:)),
            contextInfo: nil
        )
        try await waitUntil("the delegate heard") { !delegate.answers.isEmpty }
        XCTAssertEqual(delegate.answers, [true])
        XCTAssertTrue(try contents(other).contains("1,Marlow,3"))
        XCTAssertTrue(same(document.fileURL, other))

        // Export (Save To) isn't offered, and NSDocument's own writing is
        // refused.
        let export = NSMenuItem(title: "", action: #selector(NSDocument.saveTo(_:)), keyEquivalent: "")
        XCTAssertFalse(document.validateUserInterfaceItem(export))
        let refused = expectation(description: "refused")
        document.save(to: directory.appending(path: "x.csv"), ofType: "public.comma-separated-values-text", for: .saveToOperation) { error in
            XCTAssertNotNil(error)
            refused.fulfill()
        }
        await fulfillment(of: [refused], timeout: 5)
        XCTAssertFalse(FileManager.default.fileExists(atPath: directory.appending(path: "x.csv").path(percentEncoded: false)))
    }

    // MARK: While the file is read again (task 2.5.3c review)

    /// Save, Save As and Duplicate are off while a Reload reads the file
    /// again (held in the core's open), and asked anyway they do nothing:
    /// no panel, nothing written. Once the Reload is over, Save As works.
    func testSaveAsAndDuplicateWaitForAReload() async throws {
        let url = try file("reloading.csv", csv)
        let threads = OpenThreads()
        let gate = OpenGate()
        hookOpens(threads, gate: gate)
        let (document, model, content) = try await open(url)
        gate.hold()
        content.reload(.reload)
        let reloading = try XCTUnwrap(content.reloading)
        try await waitUntil("the Reload is reading") { gate.isWaiting }

        let items = [
            #selector(NSDocument.save(_:)), #selector(NSDocument.saveAs(_:)), #selector(NSDocument.duplicate(_:)),
            #selector(CSVDocument.saveDuplicate(_:)),
        ].map { NSMenuItem(title: "", action: $0, keyEquivalent: "") }
        XCTAssertEqual(items.map(document.validateUserInterfaceItem), [false, false, false, false])
        let copy = directory.appending(path: "reloading copy.csv")
        destination = copy
        document.saveAs(nil)
        document.saveDuplicate(nil)
        XCTAssertTrue(panels.isEmpty, "no panel")
        let refused = await document.saveAs(to: copy).value
        XCTAssertFalse(refused)
        XCTAssertNil(document.saving)

        gate.release()
        await reloading.value
        XCTAssertFalse(FileManager.default.fileExists(atPath: copy.path(percentEncoded: false)))
        XCTAssertTrue(same(document.fileURL, url))
        set(model, 0, 1, "Marlowe")
        let saved = try await saveAs(document, to: copy)
        XCTAssertTrue(saved)
        XCTAssertTrue(same(document.fileURL, copy))
    }

    /// A Save As that ends after a Reload replaced the core document it
    /// saved (one that got past the guards above) writes its copy, but the
    /// document doesn't follow it: it stays the file the Reload read, and
    /// the model's save ends.
    func testASaveAsThatEndsAfterAReloadIsNotFollowed() async throws {
        let url = try file("raced.csv", csv)
        let (document, model, _) = try await open(url)
        set(model, 0, 1, "before")
        debugHoldNextSave()
        let copy = directory.appending(path: "raced copy.csv")
        destination = copy
        document.saveAs(nil)
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        let handle = model.readingID.handle
        let reloaded = try await model.reloadInBackground()
        XCTAssertTrue(reloaded)
        XCTAssertNotEqual(model.readingID.handle, handle)

        debugReleaseHeldSave()
        _ = await saving.value
        XCTAssertTrue(try contents(copy).contains("1,before,3"), "the copy is written")
        XCTAssertTrue(same(document.fileURL, url), "the document doesn't follow it")
        XCTAssertTrue(same(model.url, url))
        XCTAssertEqual(document.displayName, "raced.csv")
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        XCTAssertFalse(model.isSaving)
        XCTAssertFalse(model.saveOutcomePending)
        XCTAssertTrue(recent.isEmpty)
    }

    /// AppKit's `revert(toContentsOf:ofType:)` leaves unsaved edits alone,
    /// and an edit made after it checked, before its reading is adopted,
    /// stops the reading (`EditedDuringReload`) instead of being thrown
    /// away.
    func testAppKitsRevertNeverThrowsEditsAway() async throws {
        let url = try file("appkit.csv", csv)
        let threads = OpenThreads()
        hookOpens(threads)
        let (document, model, _) = try await open(url)
        let opens = threads.onMainThread.count
        try Data("id,name,qty\n9,Other,1\n".utf8).write(to: url)

        set(model, 0, 1, "Marlowe")
        try document.revert(toContentsOf: url, ofType: "public.comma-separated-values-text")
        XCTAssertFalse(model.isReloading)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(threads.onMainThread.count, opens, "not read again")
        XCTAssertEqual(value(model, 0, 1), "Marlowe")

        set(model, 0, 1, "Marlow")
        XCTAssertFalse(model.hasUnsavedEdits)
        try document.revert(toContentsOf: url, ofType: "public.comma-separated-values-text")
        // Before the revert's task has run.
        set(model, 0, 1, "During")
        try await waitUntil("read again") { threads.onMainThread.count == opens + 1 }
        try await waitUntil("the reading is over") { !model.isReloading }
        XCTAssertEqual(value(model, 0, 1), "During")
        XCTAssertEqual(value(model, 0, 0), "1", "the new reading isn't adopted")
        XCTAssertTrue(document.isDocumentEdited)
    }

    // MARK: Revert to Saved (ADR-0008 decisions 4 and 10)

    /// Revert asks before discarding the edits, then reads the file again
    /// off the main thread, as Reload does, never through AppKit's
    /// `read(from:)`. No Versions: no autosave in place, no versions kept.
    func testRevertAsksThenReadsTheFileAgainOffTheMainThread() async throws {
        let url = try file("revert.csv", csv)
        let threads = OpenThreads()
        DocumentModel.openForTesting = { path, environment, options, observer in
            threads.record()
            return try openDocument(
                path: path,
                volume: TemporaryFolders.volume(for: URL(filePath: path)),
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer
            )
        }
        let (document, model, content) = try await open(url)
        XCTAssertFalse(CSVDocument.autosavesInPlace)
        XCTAssertFalse(CSVDocument.preservesVersions)
        set(model, 0, 1, "Marlowe")
        let opensBefore = threads.onMainThread.count

        answers = [1] // Cancel.
        document.revertToSaved(nil)
        XCTAssertEqual(alerts.map(\.messageText), ["Revert “revert.csv” to the saved version and discard your changes?"])
        XCTAssertEqual(alerts.first?.buttons.map(\.title), ["Revert", "Cancel"])
        XCTAssertNil(content.reloading)
        XCTAssertEqual(value(model, 0, 1), "Marlowe")

        answers = [0] // Revert.
        document.revertToSaved(nil)
        let reverting = try XCTUnwrap(content.reloading)
        await reverting.value
        XCTAssertEqual(value(model, 0, 1), "Marlow")
        XCTAssertFalse(document.isDocumentEdited)
        XCTAssertFalse(document.history.undoManager.canUndo)
        XCTAssertTrue(document.history.journal.isEmpty)
        XCTAssertEqual(Array(threads.onMainThread.dropFirst(opensBefore)), [false], "read again once, off the main thread")

        // AppKit's revert reads off the main thread too.
        try Data("id,name,qty\n9,Other,1\n".utf8).write(to: url)
        try document.revert(toContentsOf: url, ofType: "public.comma-separated-values-text")
        try await waitUntil("read again") { value(model, 0, 1) == "Other" }
        XCTAssertEqual(Array(threads.onMainThread.dropFirst(opensBefore)), [false, false])
        XCTAssertEqual(alerts.count, 2, "no more questions")
    }

    /// A drive coming back keeps the edits (ADR-0008 decision 4): Leal has
    /// confirmed the file is unchanged.
    func testADriveComingBackKeepsTheEdits() async throws {
        simulateDrive(.disconnect(at: 100_000))
        let url = try file("back.csv", text(rows: 20_000))
        let (document, model, _) = try await open(url, indexed: false)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        set(model, 0, 1, "edited")
        let generation = model.generation
        _ = model.call { $0.debugSimulateDriveBack() }
        await model.checkOriginal()?.value
        try await waitUntil("reconnected") { model.storage != .disconnected }
        XCTAssertGreaterThan(model.generation, generation)
        XCTAssertEqual(value(model, 0, 1), "edited")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertTrue(document.history.undoManager.canUndo)
    }

    /// A save that ends with `SaveJob.restarted()` set (a drive back
    /// during it, reconnected as it ended) takes up the new reading as
    /// `checkOriginal`'s restart does, here after a cancelled Save As: the
    /// drive is no longer disconnected, the file is read on, and the edits
    /// stay.
    func testASaveThatEndsWithARestartTakesUpTheNewReading() async throws {
        simulateDrive(.disconnect(at: 100_000))
        let url = try file("restart.csv", text(rows: 20_000))
        let (document, model, content) = try await open(url, indexed: false)
        try await waitUntil("disconnected") { model.storage == .disconnected }
        set(model, 0, 1, "edited")
        let generation = model.generation
        debugHoldNextSave()
        destination = directory.appending(path: "restart copy.csv")
        document.saveAs(nil)
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        let job = try XCTUnwrap(model.saveJob)
        // The drive is back: the check waits for the save.
        _ = model.call { $0.debugSimulateDriveBack() }
        await model.checkOriginal()?.value
        XCTAssertEqual(model.generation, generation, "not while the save runs")
        content.cancelOperation(nil)
        debugReleaseHeldSave()
        let saved = await saving.value

        XCTAssertFalse(saved)
        XCTAssertNotNil(job.restarted(), "reconnected as the save ended")
        XCTAssertEqual(model.generation, job.restarted())
        XCTAssertNotEqual(model.storage, .disconnected)
        XCTAssertEqual(value(model, 0, 1), "edited")
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertTrue(same(document.fileURL, url))
        try await waitUntil("read on") { model.isIndexComplete }
        XCTAssertEqual(model.rowCount, 20_000)
    }

    // MARK: The main thread never waits (DESIGN §3.9)

    /// With a Save As held after its snapshot, inside its file access and
    /// coordinated write, the main thread edits, reads, and asks
    /// `NSDocument` what it knows, at once; a background thread lets the
    /// save go after a second, so a wait would show as a slow answer, not a
    /// hang. Save As, Duplicate and Revert are off meanwhile.
    func testTheMainThreadNeverWaitsForASaveAs() async throws {
        let url = try file("held.csv", csv)
        let (document, model, content) = try await open(url)
        set(model, 0, 1, "before")
        debugHoldNextSave()
        let copy = directory.appending(path: "held copy.csv")
        destination = copy
        let asked = Date()
        document.saveAs(nil)
        XCTAssertLessThan(Date().timeIntervalSince(asked), 0.5, "Save As returns at once")
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") { model.saveJob?.progress().snapshotVersion != nil }
        try await waitUntil("the progress shows") { model.saveProgress != nil }
        content.view.layoutSubtreeIfNeeded()
        XCTAssertTrue(content.statusBar.text.contains("Saving…"), content.statusBar.text)

        Thread.detachNewThread {
            Thread.sleep(forTimeInterval: 1)
            debugReleaseHeldSave()
        }
        let started = Date()
        set(model, 1, 1, "during")
        XCTAssertEqual(value(model, 1, 1), "during")
        _ = document.fileURL
        _ = document.isDocumentEdited
        _ = document.fileModificationDate
        _ = document.displayName
        let items = [
            #selector(NSDocument.saveAs(_:)), #selector(NSDocument.duplicate(_:)), #selector(CSVDocument.saveDuplicate(_:)),
            #selector(NSDocument.revertToSaved(_:)),
        ].map { NSMenuItem(title: "", action: $0, keyEquivalent: "") }
        XCTAssertEqual(items.map(document.validateUserInterfaceItem), [false, false, false, false])
        XCTAssertLessThan(Date().timeIntervalSince(started), 0.5, "nothing waited for the save")

        let saved = await saving.value
        XCTAssertTrue(saved)
        XCTAssertTrue(try contents(copy).contains("1,before,3") && !(try contents(copy).contains("during")))
        XCTAssertTrue(same(document.fileURL, copy))
        XCTAssertTrue(document.isDocumentEdited, "the edit made during the save is unsaved")
        XCTAssertEqual(value(model, 1, 1), "during")
        XCTAssertEqual(document.history.journal.count, 1)
    }
}

/// Which threads the core's documents were opened on, in order.
private final class OpenThreads: @unchecked Sendable {
    // @unchecked: `threads` is only touched with `lock` held.
    private let lock = NSLock()
    private var threads: [Bool] = []

    func record() {
        let onMain = Thread.isMainThread
        lock.withLock { threads.append(onMain) }
    }

    var onMainThread: [Bool] { lock.withLock { threads } }
}

/// Holds the core's opens (`hookOpens`) while held, off the main thread.
private final class OpenGate: @unchecked Sendable {
    // @unchecked: the state is only touched with `condition` locked.
    private let condition = NSCondition()
    private var held = false
    private var waiting = false

    func hold() { condition.withLock { held = true } }

    func release() {
        condition.withLock {
            held = false
            condition.broadcast()
        }
    }

    /// An open is waiting at the gate.
    var isWaiting: Bool { condition.withLock { waiting } }

    func passThrough() {
        condition.withLock {
            while held {
                waiting = true
                condition.wait()
            }
            waiting = false
        }
    }
}

/// A delegate for `runModalSavePanel(for:delegate:didSave:contextInfo:)`.
private final class SaveAsDelegate: NSObject {
    private(set) var answers: [Bool] = []

    @objc func document(_ document: NSDocument, didSave: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(didSave)
    }
}
