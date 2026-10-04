import AppKit
import LealFFI
import XCTest
import os

@testable import Leal

/// Task 2.5.3a against the real core, hosted (and sandboxed) in Leal.app:
/// Save goes through the core inside a coordinated write; the bytes are
/// the core's own save's; the dirty state, change-count tokens and the
/// journal follow the save's snapshot, so an edit made during a save stays
/// unsaved; the main thread never waits (a save held part-way by
/// `debugHoldNextSave`); progress shows in the status bar and the file
/// isn't read again meanwhile; cancelling writes nothing; and each refusal
/// has its words and choices: locked, read-only, changed elsewhere,
/// unencodable cells, missing.
///
/// No sheet is shown (`CSVDocument.showSheet` is replaced) and nothing is
/// sent to the system (CLAUDE.md).
@MainActor
final class SaveTests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-save-\(UUID().uuidString)")
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
        debugReleaseHeldSave()
        if let savedEnvironment { CSVDocument.environment = savedEnvironment }
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        // Locked or read-only files can't be removed as they are.
        let files = (try? FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: nil)) ?? []
        for url in files {
            _ = CSVDocument.unlock(url)
            _ = chmod(url.path(percentEncoded: false), 0o644)
        }
        try? FileManager.default.removeItem(at: directory)
    }

    // MARK: Helpers

    private let csv = "id,name,qty\r\n1,Marlow,3\r\n2,\"Ostrava\",5\r\n3,Halden,8\r\n"

    private func file(_ name: String, _ bytes: Data) throws -> URL {
        let url = directory.appending(path: name)
        try bytes.write(to: url)
        return url
    }

    private func file(_ name: String, _ text: String) throws -> URL {
        try file(name, Data(text.utf8))
    }

    private struct Opened {
        let document: CSVDocument
        let model: DocumentModel
        let content: DocumentViewController
        let window: NSWindow
    }

    /// Opens `url` with a window, its sheets collected in `alerts` and
    /// answered by `answer` (the button's index; the last, the way out, by
    /// default).
    private func open(_ url: URL) async throws -> Opened {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        let window = try XCTUnwrap(controller.window)
        let content = controller.content
        content.view.layoutSubtreeIfNeeded()
        let model = try XCTUnwrap(document.model)
        try await waitUntil("indexed") { model.isIndexComplete }
        document.showSheet = { [weak self] alert, _, done in
            guard let self else { return }
            alerts.append(alert)
            let index = answers.isEmpty ? alert.buttons.count - 1 : answers.removeFirst()
            done(NSApplication.ModalResponse(rawValue: NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + index))
        }
        document.saveFollowUpForTesting = { [weak self] choice in self?.followUps.append(choice) }
        return Opened(document: document, model: model, content: content, window: window)
    }

    /// The sheets shown, in order.
    private var alerts: [NSAlert] = []
    /// The buttons to answer the next sheets with, by index.
    private var answers: [Int] = []
    /// What the alerts' Duplicate, Save As… and Save As UTF-8… did.
    private var followUps: [SaveChoice] = []

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

    /// The bytes the core's own save writes for `edits` (row, column,
    /// value) on a copy of `bytes`: `DocumentModel.save`, without
    /// `NSDocument`, the coordination or the window.
    private func coreSave(_ bytes: Data, _ edits: [(Int, Int, String)]) async throws -> Data {
        let url = try file("core-\(UUID().uuidString).csv", bytes)
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        let model = try XCTUnwrap(document.model)
        model.start()
        try await waitUntil("indexed") { model.isIndexComplete }
        for (row, column, value) in edits { set(model, row, column, value) }
        _ = try await model.save(to: url, kind: .save)
        document.close()
        return try Data(contentsOf: url)
    }

    private func modificationDate(_ url: URL) throws -> Date? {
        try FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))[.modificationDate] as? Date
    }

    private func flags(_ url: URL) -> UInt32 {
        var info = stat()
        _ = stat(url.path(percentEncoded: false), &info)
        return info.st_flags
    }

    // MARK: Saving

    /// ⌘S writes what the core's own save writes, and the document is
    /// clean: the change count, "— Edited", the journal (nothing left to
    /// replay) and `fileModificationDate`; the undo history stays.
    func testSaveWritesTheCoresBytesAndTheDocumentIsClean() async throws {
        let bytes = Data(csv.utf8)
        let url = try file("orders.csv", bytes)
        let edits = [(0, 1, "Marlow, Ltd"), (1, 1, "Ostrava \"East\""), (2, 2, "9")]
        let expected = try await coreSave(bytes, edits)
        let opened = try await open(url)
        for (row, column, value) in edits { set(opened.model, row, column, value) }
        XCTAssertTrue(opened.document.isDocumentEdited)
        XCTAssertEqual(opened.document.history.journal.count, 3)

        let saved = try await save(opened)

        XCTAssertTrue(saved)
        XCTAssertEqual(try Data(contentsOf: url), expected)
        XCTAssertEqual(
            String(decoding: expected, as: UTF8.self),
            "id,name,qty\r\n1,\"Marlow, Ltd\",3\r\n2,\"Ostrava \"\"East\"\"\",5\r\n3,Halden,9\r\n"
        )
        XCTAssertFalse(opened.document.isDocumentEdited)
        XCTAssertFalse(opened.model.hasUnsavedEdits)
        XCTAssertTrue(opened.document.history.journal.isEmpty, "the saved edits aren't replayed by Recover changes")
        XCTAssertEqual(opened.document.fileModificationDate, try modificationDate(url))
        XCTAssertTrue(opened.document.history.undoManager.canUndo, "undo carries on after a save")
        XCTAssertTrue(alerts.isEmpty)
        XCTAssertNil(opened.window.attachedSheet)
        XCTAssertNil(opened.document.saving)
    }

    /// The main thread never waits for a save (DESIGN §3.9): with the save
    /// held after its snapshot of the edits, the window edits, reads and
    /// shows progress; the file isn't read again meanwhile (Reload, Treat
    /// As, Reopen with Encoding and the Header row are off). An edit made
    /// then isn't in the file and stays unsaved: the token noted at the
    /// save's snapshot keeps the document edited, and the journal keeps
    /// that edit only.
    func testAnEditDuringASaveStaysUnsavedAndTheMainThreadNeverWaits() async throws {
        let url = try file("held.csv", csv)
        let opened = try await open(url)
        let content = opened.content
        set(opened.model, 0, 1, "before")
        debugHoldNextSave()
        let asked = Date()
        opened.document.save(nil)
        XCTAssertLessThan(Date().timeIntervalSince(asked), 0.5, "⌘S returns at once")
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") {
            opened.model.saveJob?.progress().snapshotVersion != nil
        }
        // The status bar says so, with its progress, and nothing reads
        // the file again.
        try await waitUntil("the progress shows") { opened.model.saveProgress != nil }
        content.view.layoutSubtreeIfNeeded()
        XCTAssertTrue(content.statusBar.text.contains("Saving…"), content.statusBar.text)
        XCTAssertTrue(content.statusBar.isShowingProgress)
        let header = NSMenuItem(title: "", action: #selector(DocumentViewController.toggleHeaderRow(_:)), keyEquivalent: "")
        let treatAs = NSMenuItem(title: "", action: #selector(DocumentViewController.treatAsDelimiter(_:)), keyEquivalent: "")
        let reload = NSMenuItem(title: "", action: #selector(DocumentViewController.reloadFromDisk(_:)), keyEquivalent: "")
        XCTAssertEqual([header, treatAs, reload].map(content.validateMenuItem), [false, false, false])
        XCTAssertEqual(header.toolTip, "Wait for the save to finish.")
        let saveItem = NSMenuItem(title: "", action: #selector(NSDocument.save(_:)), keyEquivalent: "")
        XCTAssertFalse(opened.document.validateUserInterfaceItem(saveItem), "one Save at a time")

        // An edit and a read now: neither waits for the save.
        let started = Date()
        set(opened.model, 1, 1, "during")
        XCTAssertEqual(value(opened.model, 1, 1), "during")
        XCTAssertLessThan(Date().timeIntervalSince(started), 0.5)
        XCTAssertNil(opened.window.attachedSheet)

        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)
        let text = try String(contentsOf: url, encoding: .utf8)
        XCTAssertTrue(text.contains("1,before,3") && !text.contains("during"), text)
        XCTAssertTrue(opened.document.isDocumentEdited, "the edit made during the save is unsaved")
        XCTAssertTrue(opened.model.hasUnsavedEdits)
        XCTAssertEqual(value(opened.model, 1, 1), "during")
        XCTAssertEqual(opened.document.history.journal.map(\.direction), [.edit], "only the edit after the snapshot")
        XCTAssertNil(opened.model.saveJob)
        content.view.layoutSubtreeIfNeeded()
        XCTAssertFalse(content.statusBar.text.contains("Saving…"))
        XCTAssertEqual([header, treatAs].map(content.validateMenuItem), [true, false], "Treat As waits for the edit to be saved")

        // Saving again saves it, with no prompt.
        try await save(opened)
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("2,\"during\",5"), "a quoted field stays quoted")
        XCTAssertFalse(opened.document.isDocumentEdited)
        XCTAssertTrue(alerts.isEmpty, alerts.map(\.messageText).joined())
    }

    /// Cancelling a save (⌘. or Escape) writes nothing, says nothing, and
    /// leaves the edits unsaved.
    func testCancellingASaveWritesNothing() async throws {
        let url = try file("cancel.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "cancelled")
        debugHoldNextSave()
        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("the save took its snapshot") {
            opened.model.saveJob?.progress().snapshotVersion != nil
        }
        // A second Save waits for the first; ⌘. stops both, the one
        // running too.
        let delegate = SaveDelegate()
        opened.document.save(withDelegate: delegate, didSave: #selector(SaveDelegate.document(_:didSave:contextInfo:)), contextInfo: nil)
        XCTAssertNotEqual(opened.document.saving, saving)
        opened.content.cancelOperation(nil)
        debugReleaseHeldSave()
        let saved = await saving.value
        try await waitUntil("the second save ended") { delegate.answers.count == 1 }

        XCTAssertFalse(saved)
        XCTAssertEqual(delegate.answers, [false])
        XCTAssertNil(opened.document.saving)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), csv)
        XCTAssertTrue(opened.document.isDocumentEdited)
        XCTAssertEqual(opened.document.history.journal.count, 1)
        XCTAssertTrue(alerts.isEmpty)
        XCTAssertNil(opened.model.saveJob)
    }

    /// Closing's Save (`saveDocument(withDelegate:…)`, as `NSDocument`'s
    /// close alert calls it) saves through the core and answers its
    /// delegate.
    func testClosingsSaveSavesThroughTheCore() async throws {
        let url = try file("close.csv", csv)
        let opened = try await open(url)
        set(opened.model, 2, 1, "Tromsø")
        let delegate = SaveDelegate()
        opened.document.save(withDelegate: delegate, didSave: #selector(SaveDelegate.document(_:didSave:contextInfo:)), contextInfo: nil)
        try await waitUntil("the delegate heard") { delegate.answers.count == 1 }

        XCTAssertEqual(delegate.answers, [true])
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("3,Tromsø,8"))
        XCTAssertFalse(opened.document.isDocumentEdited)
    }

    // MARK: Refusals

    /// A file changed elsewhere since Leal opened it, before the watcher
    /// says so: the core's check finds it, Leal asks once (never
    /// `NSDocument` too), and Save Anyway writes over it.
    func testAFileChangedElsewhereAsksOnceThenSaveAnywayWritesOverIt() async throws {
        let url = try file("changed.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "ours")
        try Data("id,name,qty\r\n1,theirs,3\r\n".utf8).write(to: url)
        answers = [0]

        let saved = try await save(opened)

        XCTAssertTrue(saved)
        XCTAssertEqual(alerts.map(\.messageText), ["“changed.csv” changed on disk since Leal opened or last saved it."])
        XCTAssertEqual(alerts.first?.buttons.map(\.title), ["Save Anyway", "Cancel"])
        XCTAssertEqual(alerts.first?.buttons.first?.hasDestructiveAction, true)
        XCTAssertEqual(alerts.first?.buttons.map(\.keyEquivalent), ["", "\r"], "Return is Cancel's, not Save Anyway's")
        XCTAssertNil(opened.window.attachedSheet, "no sheet of NSDocument's own")
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,ours,3"))
        XCTAssertFalse(opened.document.isDocumentEdited)
        XCTAssertEqual(opened.document.fileModificationDate, try modificationDate(url))
    }

    /// Once the watcher has seen the change (`diverged`, which Keep
    /// Editing leaves set), Save asks before writing; Cancel writes
    /// nothing, and asks nothing more.
    func testAChangeTheWatcherSawIsAskedAboutBeforeWriting() async throws {
        let url = try file("diverged.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "ours")
        let theirs = Data("id,name,qty\r\n1,theirs,3\r\n".utf8)
        try theirs.write(to: url)
        try await waitUntil("the watcher saw it") {
            opened.model.checkOriginal()
            return opened.model.original.diverged
        }

        let saved = try await save(opened)

        XCTAssertFalse(saved)
        XCTAssertEqual(alerts.map(\.messageText), ["“diverged.csv” changed on disk since Leal opened or last saved it."])
        XCTAssertEqual(try Data(contentsOf: url), theirs)
        XCTAssertTrue(opened.document.isDocumentEdited)
    }

    /// A locked file: Unlock clears the lock and saves; Duplicate leaves
    /// it locked and keeps the changes in a copy.
    func testALockedFileOffersUnlockAndDuplicate() async throws {
        let url = try file("locked.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "unlocked")
        XCTAssertEqual(chflags(url.path(percentEncoded: false), UInt32(UF_IMMUTABLE)), 0)

        answers = [1]
        let refused = try await save(opened)
        XCTAssertFalse(refused)
        XCTAssertEqual(alerts.last?.messageText, "“locked.csv” is locked.")
        XCTAssertEqual(alerts.last?.informativeText, "Unlock it to save your changes there, or keep them in a duplicate.")
        XCTAssertEqual(alerts.last?.buttons.map(\.title), ["Unlock", "Duplicate", "Cancel"])
        XCTAssertEqual(followUps, [.duplicate])
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), csv)
        XCTAssertNotEqual(flags(url) & UInt32(UF_IMMUTABLE), 0)

        answers = [0]
        let savedAgain = try await save(opened)
        XCTAssertTrue(savedAgain)
        XCTAssertEqual(flags(url) & UInt32(UF_IMMUTABLE), 0)
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,unlocked,3"))
        XCTAssertFalse(opened.document.isDocumentEdited)
        XCTAssertEqual(alerts.count, 2)
    }

    /// A file Leal may not write offers Duplicate; Cancel leaves it and
    /// the edits as they were.
    func testAFileLealMayNotWriteOffersDuplicate() async throws {
        let url = try file("read-only.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "mine")
        XCTAssertEqual(chmod(url.path(percentEncoded: false), 0o444), 0)

        let refused = try await save(opened)
        XCTAssertFalse(refused)

        XCTAssertEqual(alerts.map(\.messageText), ["You don’t have permission to save “read-only.csv”."])
        XCTAssertEqual(alerts.first?.buttons.map(\.title), ["Duplicate", "Cancel"])
        XCTAssertTrue(followUps.isEmpty)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), csv)
        XCTAssertTrue(opened.document.isDocumentEdited)
    }

    /// A value the file's encoding can't hold: the cells are named, and
    /// Save As UTF-8… is offered (F5: nothing substituted).
    func testUnencodableCellsAreNamedAndSaveAsUTF8IsOffered() async throws {
        var bytes = Data("id,name\n1,Caf".utf8)
        bytes.append(contentsOf: [0xE9])
        bytes.append(Data("\n2,Na".utf8))
        bytes.append(contentsOf: [0xEF])
        bytes.append(Data("ve\n".utf8))
        let url = try file("legacy.csv", bytes)
        let opened = try await open(url)
        let encoding = StatusText.encodingName(opened.model.interpretation.encoding)
        XCTAssertNotEqual(opened.model.interpretation.encoding, .utf8)
        set(opened.model, 0, 1, "Café 😀")
        set(opened.model, 1, 1, "Naïve ✓")
        answers = [0]

        let refused = try await save(opened)
        XCTAssertFalse(refused)

        let alert = try XCTUnwrap(alerts.first)
        XCTAssertEqual(alert.messageText, "Some cells can’t be saved in \(encoding).")
        XCTAssertEqual(
            alert.informativeText,
            "• Row 1, column 2\n• Row 2, column 2\n\nThey hold characters \(encoding) can’t represent, and Leal never replaces them. Change them, or save a UTF-8 copy of “legacy.csv”."
        )
        XCTAssertEqual(alert.buttons.map(\.title), ["Save As UTF-8…", "Cancel"])
        XCTAssertEqual(followUps, [.saveAsUTF8])
        XCTAssertEqual(try Data(contentsOf: url), bytes)
    }

    /// The first eight cells are named; the rest are counted, or, when
    /// the core stopped counting at 1,000 (`more`), "and more".
    func testUnencodableNamesEightCellsThenCountsOrSaysAndMore() throws {
        let cells = (1...10).map { CellPlace(row: UInt64($0), column: 0) }
        let counted = try XCTUnwrap(SaveText.saveFailure(.Unencodable(encoding: .windows1252, cells: cells, more: false), name: "a.csv", headerRows: 1))
        let lines = counted.detail.components(separatedBy: "\n")
        XCTAssertEqual(lines.prefix(9), ["• Row 1, column 1", "• Row 2, column 1", "• Row 3, column 1", "• Row 4, column 1", "• Row 5, column 1", "• Row 6, column 1", "• Row 7, column 1", "• Row 8, column 1", "and 2 more"])
        let thousand = (1...1000).map { CellPlace(row: UInt64($0), column: 0) }
        let more = try XCTUnwrap(SaveText.saveFailure(.Unencodable(encoding: .windows1252, cells: thousand, more: true), name: "a.csv", headerRows: 1))
        XCTAssertEqual(more.detail.components(separatedBy: "\n")[8], "and more")
        XCTAssertEqual(more.title, "Some cells can’t be saved in Windows-1252.")
        let header = try XCTUnwrap(SaveText.saveFailure(.Unencodable(encoding: .windows1252, cells: [CellPlace(row: 0, column: 2)], more: false), name: "a.csv", headerRows: 1))
        XCTAssertTrue(header.detail.hasPrefix("• The header row, column 3\n\n"), header.detail)
    }

    /// A file deleted since it was opened: Save says it can't be found and
    /// offers Save As; it never asks about a change first.
    func testAMissingFileOffersSaveAs() async throws {
        let url = try file("gone.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "kept")
        try FileManager.default.removeItem(at: url)
        answers = [0]

        let refused = try await save(opened)
        XCTAssertFalse(refused)

        XCTAssertEqual(alerts.map(\.messageText), ["“gone.csv” can’t be found."])
        XCTAssertEqual(alerts.first?.buttons.map(\.title), ["Save As…", "Cancel"])
        XCTAssertEqual(followUps, [.saveAs])
        XCTAssertFalse(FileManager.default.fileExists(atPath: url.path(percentEncoded: false)))
        XCTAssertTrue(opened.document.isDocumentEdited)
    }

    /// Every failure has words of its own, none of them the core's
    /// English; a cancel and a failed document (which says so itself) say
    /// nothing.
    func testEveryFailureIsWordedWithoutTheCoresEnglish() throws {
        let failures: [SaveFailure] = [
            .ReadOnly, .TooLarge(byteCount: 5_000_000_000), .Incomplete, .Unavailable, .ChangedElsewhere, .Missing, .Moving,
            .NotWritable, .Locked, .NotAFile, .DriveDisconnected, .ChangedOnDisk, .DeletedElsewhere,
            .Io(step: "writing the new file", code: 28, message: "No space left on device (os error 28)"),
            .Internal(message: "boom"), .Unconvertible(encoding: .utf16Le, cells: [], more: false),
        ]
        var details: Set<String> = []
        for failure in failures {
            let refusal = try XCTUnwrap(SaveText.saveFailure(failure, name: "a.csv", headerRows: 1), "\(failure)")
            XCTAssertFalse(refusal.title.isEmpty || refusal.detail.isEmpty, "\(failure)")
            XCTAssertFalse(refusal.detail.contains("os error") || refusal.detail.contains("boom"), refusal.detail)
            XCTAssertTrue([.cancel, .ok].contains(refusal.choices.last), "\(failure): the last choice is the way out")
            details.insert(refusal.detail)
        }
        // The drive-and-read failures share theirs; the rest differ.
        // and so do Leal's own problems.
        XCTAssertEqual(details.count, failures.count - 4)
        XCTAssertNil(SaveText.saveFailure(.Cancelled, name: "a.csv", headerRows: 1))
        XCTAssertNil(SaveText.saveFailure(.DocumentFailed(message: "boom"), name: "a.csv", headerRows: 1))
        XCTAssertEqual(SaveText.saveFailure(.Moving, name: "a.csv", headerRows: 1)?.choices, [.tryAgain, .cancel])
        XCTAssertEqual(SaveText.saveFailure(.ReadOnly, name: "a.csv", headerRows: 1)?.choices, [.saveAsUTF8, .cancel])
    }

    // MARK: While a save runs (task 2.5.3a review)

    /// `NSDocument` reads its own `fileURL`, `isDocumentEdited` and
    /// `fileModificationDate` inside a synchronous file access on the main
    /// thread, which waits for the save's asynchronous one. So the save
    /// must end its file access without needing the main thread: here the
    /// main thread asks everything that might wait, with the save held,
    /// while a background thread lets the save go on. A deadlock would
    /// hang the main thread for good: the watchdog then stops the test
    /// host, so it fails rather than hangs.
    func testTheMainThreadAskingDuringASaveNeverDeadlocks() async throws {
        let url = try file("deadlock.csv", csv)
        let opened = try await open(url)
        let document = opened.document
        let controller = try XCTUnwrap(NSDocumentController.shared as? DocumentController)
        controller.addDocument(document)
        document.unsavedChangesPromptForTesting = { answer in answer(false) }
        set(opened.model, 0, 1, "held")
        debugHoldNextSave()
        document.save(nil)
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") {
            opened.model.saveJob?.progress().snapshotVersion != nil
        }

        let watchdog = Watchdog(seconds: 30, what: "the main thread, asking NSDocument during a save")
        DispatchQueue.global().asyncAfter(deadline: .now() + 0.3) { debugReleaseHeldSave() }
        // Synchronously, on the main thread, for a second: before the
        // save goes on, and after it has written the file but before it
        // has ended its file access, which it mustn't need the main
        // thread for.
        let started = Date()
        let items = [
            #selector(NSDocument.revertToSaved(_:)), #selector(NSDocument.saveAs(_:)), #selector(NSDocument.duplicate(_:)),
            #selector(NSDocument.move(_:)), #selector(NSDocument.rename(_:)), #selector(NSDocument.lock(_:)),
        ]
        let probe = SaveCloseProbe()
        document.canClose(withDelegate: probe, shouldClose: #selector(SaveCloseProbe.document(_:shouldClose:contextInfo:)), contextInfo: nil)
        document.revertToSaved(nil)
        var enabled: [Bool] = []
        while Date().timeIntervalSince(started) < 1 {
            enabled = items.map { document.validateUserInterfaceItem(NSMenuItem(title: "", action: $0, keyEquivalent: "")) }
            _ = controller.hasEditedDocuments
        }
        let asked = Date().timeIntervalSince(started)
        let saved = await saving.value
        watchdog.stop()

        XCTAssertTrue(saved)
        XCTAssertEqual(enabled, [false, false, false, false, false, false], "Revert, Save As, Duplicate, Move, Rename and Lock wait for the save")
        XCTAssertNil(document.saving)
        XCTAssertLessThan(asked, 5, "nothing waited for the save")
        try await waitUntil("closing heard") { !probe.answers.isEmpty }
        XCTAssertEqual(probe.answers, [true], "saved: nothing left to ask about")
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,held,3"))
        XCTAssertEqual(value(opened.model, 0, 1), "held", "Revert did nothing during the save")
        XCTAssertFalse(document.isDocumentEdited)
    }

    /// Closing during a save, with the cell typed back as it was in the
    /// file (so the core has nothing unsaved): the document stays edited
    /// while the save runs, and closing waits for the save before it
    /// decides. The saved file then holds the edit and the cell doesn't,
    /// so it asks.
    func testClosingDuringASaveWaitsForItThenAsks() async throws {
        let url = try file("close-held.csv", csv)
        let opened = try await open(url)
        let document = opened.document
        var asked: [Bool] = []
        document.unsavedChangesPromptForTesting = { answer in
            asked.append(document.saving == nil)
            answer(false)
        }
        set(opened.model, 0, 1, "held")
        debugHoldNextSave()
        document.save(nil)
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") {
            opened.model.saveJob?.progress().snapshotVersion != nil
        }
        set(opened.model, 0, 1, "Marlow")
        XCTAssertFalse(opened.model.hasUnsavedEdits)
        XCTAssertTrue(document.isDocumentEdited, "edited while the save runs")

        let probe = SaveCloseProbe()
        document.canClose(withDelegate: probe, shouldClose: #selector(SaveCloseProbe.document(_:shouldClose:contextInfo:)), contextInfo: nil)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertTrue(probe.answers.isEmpty, "closing waits for the save")
        XCTAssertTrue(asked.isEmpty)
        debugReleaseHeldSave()
        let saved = await saving.value
        try await waitUntil("closing heard") { !probe.answers.isEmpty }

        XCTAssertTrue(saved)
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,held,3"))
        XCTAssertEqual(asked, [true], "asked once, after the save")
        XCTAssertEqual(probe.answers, [false])
        XCTAssertTrue(document.isDocumentEdited, "the cell reads differently from the saved file")
    }

    /// Quitting during a save, with the cell typed back as it was in the
    /// file: a saving document counts as edited, so AppKit reviews the
    /// documents, and the review waits for the save before it asks; the app
    /// delegate's answer waits for it too, rather than quitting part-way.
    func testQuittingDuringASaveWaitsForItThenReviews() async throws {
        let url = try file("quit-held.csv", csv)
        let opened = try await open(url)
        let document = opened.document
        let controller = try XCTUnwrap(NSDocumentController.shared as? DocumentController)
        controller.addDocument(document)
        var asked: [Bool] = []
        document.unsavedChangesPromptForTesting = { answer in
            asked.append(document.saving == nil)
            answer(false)
        }
        set(opened.model, 0, 1, "held")
        debugHoldNextSave()
        document.save(nil)
        let saving = try XCTUnwrap(document.saving)
        try await waitUntil("the save took its snapshot") {
            opened.model.saveJob?.progress().snapshotVersion != nil
        }
        set(opened.model, 0, 1, "Marlow")
        XCTAssertTrue(controller.hasEditedDocuments, "a saving document counts as edited")

        // AppKit's review, as `NSApplication.terminate` starts it, and the
        // app delegate's answer.
        let review = SaveReviewProbe()
        controller.reviewUnsavedDocuments(
            withAlertTitle: nil,
            cancellable: true,
            delegate: review,
            didReviewAllSelector: #selector(SaveReviewProbe.documentController(_:didReviewAll:contextInfo:)),
            contextInfo: nil
        )
        let app = AppDelegate()
        var quit: [Bool] = []
        app.replyToTerminate = { quit.append($0) }
        XCTAssertEqual(app.applicationShouldTerminate(NSApp), .terminateLater, "never quits in the middle of a save")
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertTrue(review.answers.isEmpty && quit.isEmpty && asked.isEmpty, "both wait for the save")

        debugReleaseHeldSave()
        let saved = await saving.value
        XCTAssertTrue(saved)
        try await waitUntil("reviewed and answered") { !review.answers.isEmpty && !quit.isEmpty }

        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,held,3"))
        XCTAssertEqual(asked, [true, true], "each asked once, after the save")
        XCTAssertEqual(review.answers, [false])
        XCTAssertEqual(quit, [false])
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertTrue(FileManager.default.fileExists(atPath: url.path(percentEncoded: false)))
    }

    /// Another app reading the file through a coordinated read asks its
    /// presenters to save first: Leal says there is nothing to save, so
    /// `NSDocument` never writes (in place or as an autosave), and the
    /// read goes on at once with the file as it is on disk.
    func testAnotherAppsCoordinatedReadWritesNothing() async throws {
        let url = try file("read-elsewhere.csv", csv)
        let opened = try await open(url)
        let document = opened.document
        set(opened.model, 0, 1, "unsaved")
        XCTAssertNil(document.autosavingFileType)
        // The document presents its file (as it does once on screen).
        let registered = NSFileCoordinator.filePresenters.contains { $0 === document }
        if !registered { NSFileCoordinator.addFilePresenter(document) }
        defer { if !registered { NSFileCoordinator.removeFilePresenter(document) } }
        let expected = Data(csv.utf8)

        let error = await FileWork.run { () -> String? in
            var error: NSError?
            var read: Data?
            NSFileCoordinator(filePresenter: nil).coordinate(readingItemAt: url, options: [], error: &error) { url in
                read = try? Data(contentsOf: url)
            }
            if let error { return String(describing: error) }
            return read == expected ? nil : "read something else"
        }

        XCTAssertNil(error)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), csv)
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertNil(document.autosavedContentsFileURL, "no autosave elsewhere")
        XCTAssertTrue(alerts.isEmpty)
    }

    /// Save Anyway is for the file the user was asked about: after an
    /// Unlock, Save goes round again and asks again if it still differs,
    /// rather than writing over whatever is there by then.
    func testSaveAnywayIsAskedAgainAfterUnlock() async throws {
        let url = try file("changed-locked.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "ours")
        try Data("id,name,qty\r\n1,theirs,3\r\n".utf8).write(to: url)
        XCTAssertEqual(chflags(url.path(percentEncoded: false), UInt32(UF_IMMUTABLE)), 0)
        answers = [0, 0, 0]

        let saved = try await save(opened)

        XCTAssertTrue(saved)
        XCTAssertEqual(alerts.map(\.messageText), [
            "“changed-locked.csv” changed on disk since Leal opened or last saved it.",
            "“changed-locked.csv” is locked.",
            "“changed-locked.csv” changed on disk since Leal opened or last saved it.",
        ])
        XCTAssertTrue(try String(contentsOf: url, encoding: .utf8).contains("1,ours,3"))
    }

    /// ⌘. while Save waits for another app to let go of the file stops it
    /// at once: the status bar said "Waiting to save…", nothing is written,
    /// and nothing is asked.
    func testCancellingASaveWaitingForAnotherAppWritesNothing() async throws {
        let url = try file("coordinated.csv", csv)
        let opened = try await open(url)
        set(opened.model, 0, 1, "waited")
        // Another writer holds the file until `release` is signalled.
        let entered = DispatchSemaphore(value: 0)
        let release = DispatchSemaphore(value: 0)
        let other = Task.detached {
            var error: NSError?
            NSFileCoordinator(filePresenter: nil).coordinate(writingItemAt: url, options: .forReplacing, error: &error) { _ in
                entered.signal()
                release.wait()
            }
        }
        defer { release.signal() }
        await FileWork.run { entered.wait() }

        opened.document.save(nil)
        let saving = try XCTUnwrap(opened.document.saving)
        try await waitUntil("waiting to save") { opened.model.isWaitingToSave }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertNil(opened.model.saveJob, "the core's save hasn't started")
        opened.content.view.layoutSubtreeIfNeeded()
        XCTAssertTrue(opened.content.statusBar.text.contains("Waiting to save…"), opened.content.statusBar.text)

        let started = Date()
        opened.content.cancelOperation(nil)
        let saved = await saving.value
        XCTAssertLessThan(Date().timeIntervalSince(started), 2, "stops without waiting for the other app")
        release.signal()
        await other.value

        XCTAssertFalse(saved)
        XCTAssertFalse(opened.model.isWaitingToSave)
        XCTAssertEqual(try String(contentsOf: url, encoding: .utf8), csv)
        XCTAssertTrue(opened.document.isDocumentEdited)
        XCTAssertTrue(alerts.isEmpty)
    }

    // MARK: Progress

    /// The save's steps in the status bar, `Checking` (the census before
    /// writing, task 2.4c) as one of its own with its own progress.
    func testTheStatusBarShowsEachStepOfASave() {
        let checking = SavingStatus(SaveProgress(phase: .checking, written: 50, total: 200, snapshotVersion: 3))
        XCTAssertEqual(checking, SavingStatus(step: .checking, fraction: 0.25))
        XCTAssertEqual(SavingStatus(SaveProgress(phase: .queued, written: 0, total: 0, snapshotVersion: nil)).fraction, nil)
        XCTAssertEqual(SavingStatus(SaveProgress(phase: .replacing, written: 9, total: 9, snapshotVersion: 3)).step, .finishing)

        var status = StatusSummary(
            rows: 3, columns: 3, indexing: false, fractionIndexed: 1, delimiter: .comma, lineEnding: .crlf,
            encoding: .utf8, encodingSource: .guess, header: true, headerSource: .guess, readOnly: false
        )
        status.saving = checking
        XCTAssertEqual(StatusText.items(status)[1].text, "Checking the file before saving…")
        let bar = StatusBarView()
        bar.show(status)
        XCTAssertTrue(bar.isShowingProgress)
        XCTAssertEqual(bar.progressShown.value, 0.25)
        XCTAssertEqual(bar.progressShown.percent, StatusText.percent(0.25))
        status.saving = SavingStatus(step: .writing, fraction: 0.5)
        XCTAssertEqual(StatusText.items(status)[1].text, "Saving…")
        status.saving = nil
        bar.show(status)
        XCTAssertFalse(bar.isShowingProgress)
    }
}

/// `canClose`'s delegate, which hears the answer.
@MainActor
private final class SaveCloseProbe: NSObject {
    private(set) var answers: [Bool] = []

    @objc func document(_ document: NSDocument, shouldClose: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(shouldClose)
    }
}

/// `reviewUnsavedDocuments`'s delegate, which hears the answer.
@MainActor
private final class SaveReviewProbe: NSObject {
    private(set) var answers: [Bool] = []

    @objc func documentController(_ controller: NSDocumentController, didReviewAll: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(didReviewAll)
    }
}

/// Stops the test host if `stop()` isn't called within `seconds`: a main
/// thread deadlocked for good would otherwise hang the test run.
private final class Watchdog: Sendable {
    private let stopped = OSAllocatedUnfairLock(initialState: false)

    init(seconds: Double, what: String) {
        DispatchQueue.global().asyncAfter(deadline: .now() + seconds) { [stopped] in
            if !stopped.withLock({ $0 }) {
                fatalError("deadlocked: \(what)")
            }
        }
    }

    func stop() {
        stopped.withLock { $0 = true }
    }
}

/// A delegate for `saveDocument(withDelegate:didSave:contextInfo:)`.
@MainActor
private final class SaveDelegate: NSObject {
    var answers: [Bool] = []

    @objc func document(_ document: NSDocument, didSave: Bool, contextInfo: UnsafeMutableRawPointer?) {
        answers.append(didSave)
    }
}
