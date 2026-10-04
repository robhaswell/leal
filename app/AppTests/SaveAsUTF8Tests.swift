import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Task 2.3 against the real core, hosted (and sandboxed) in Leal.app: the
/// UTF-16 banner's **Save As UTF-8…** (mockup 06a) saves a UTF-8 copy
/// through the core, and the window then shows the copy, which isn't
/// read-only; bytes that can't be converted are named, and nothing is
/// written (ADR-0008 decision 7, F5).
@MainActor
final class SaveAsUTF8Tests: XCTestCase {
    private var directory: URL!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-utf8-\(UUID().uuidString)")
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
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        try? FileManager.default.removeItem(at: directory)
    }

    /// `text` as UTF-16 LE with its BOM, at `name` in the test's folder.
    private func utf16File(_ name: String, _ text: String) throws -> (URL, Data) {
        var data = Data([0xFF, 0xFE])
        data.append(try XCTUnwrap(text.data(using: .utf16LittleEndian)))
        let url = directory.appending(path: name)
        try data.write(to: url)
        return (url, data)
    }

    private func open(_ url: URL) throws -> (CSVDocument, DocumentModel, DocumentWindowController) {
        let document = try CSVDocument(contentsOf: url, ofType: "public.comma-separated-values-text")
        document.makeWindowControllers()
        let controller = try XCTUnwrap(document.windowControllers.first as? DocumentWindowController)
        _ = controller.window
        return (document, try XCTUnwrap(document.model), controller)
    }

    func testTheBannerButtonSavesAUTF8CopyAndTheWindowShowsIt() async throws {
        let (url, data) = try utf16File("legacy.csv", "id\tname\r\n1\tZoë 😀\r\n")
        let (document, model, controller) = try open(url)
        let content = controller.content
        XCTAssertTrue(model.isReadOnly)
        XCTAssertNotNil(controller.lock)
        let banner = try XCTUnwrap(content.readOnlyBanner)

        let copy = directory.appending(path: "legacy (UTF-8).csv")
        var suggested: (name: String, folder: URL)?
        content.chooseUTF8Destination = { name, folder, _, done in
            suggested = (name, folder)
            done(copy)
        }
        try XCTUnwrap(banner.button).performClick(nil)
        await content.savingAsUTF8?.value

        XCTAssertEqual(suggested?.name, "legacy (UTF-8).csv")
        XCTAssertEqual(suggested?.folder.standardizedFileURL, directory.standardizedFileURL)
        XCTAssertEqual(try Data(contentsOf: copy), Data("\u{FEFF}id\tname\r\n1\tZoë 😀\r\n".utf8))
        XCTAssertEqual(try Data(contentsOf: url), data, "the UTF-16 file is untouched")
        // The window is the copy now, in UTF-8, and not read-only.
        XCTAssertEqual(document.fileURL?.standardizedFileURL, copy.standardizedFileURL)
        XCTAssertEqual(model.url.standardizedFileURL, copy.standardizedFileURL)
        XCTAssertEqual(model.interpretation.encoding, .utf8)
        XCTAssertFalse(model.isReadOnly)
        XCTAssertNil(controller.lock)
        XCTAssertNil(content.readOnlyBanner)
        XCTAssertEqual(model.cell(row: 0, column: 1), .text("Zoë 😀", truncated: false))
        document.close()
    }

    /// An unpaired surrogate: the alert names its cell, nothing is written,
    /// and the window still shows the UTF-16 file.
    func testBytesThatCantBeConvertedAreNamedAndNothingIsWritten() async throws {
        let (url, _) = try utf16File("broken.csv", "id\tname\r\n1\tX\r\n")
        var bytes = try Data(contentsOf: url)
        let x = try XCTUnwrap(bytes.firstIndex(of: UInt8(ascii: "X")))
        bytes.replaceSubrange(x..<x + 2, with: [0x3D, 0xD8])
        try bytes.write(to: url)
        let (document, model, controller) = try open(url)
        let content = controller.content
        let copy = directory.appending(path: "broken (UTF-8).csv")
        content.chooseUTF8Destination = { _, _, _, done in done(copy) }
        var shown: NSAlert?
        content.showAlert = { alert, _ in shown = alert }
        content.saveAsUTF8(nil)
        await content.savingAsUTF8?.value

        let alert = try XCTUnwrap(shown)
        XCTAssertEqual(alert.messageText, "The UTF-8 copy wasn’t saved.")
        XCTAssertEqual(
            alert.informativeText,
            "The cell at row 1, column 2 holds bytes that aren’t UTF-16 LE text, so they can’t be converted. Leal never replaces them."
        )
        XCTAssertFalse(FileManager.default.fileExists(atPath: copy.path(percentEncoded: false)))
        XCTAssertEqual(document.fileURL?.standardizedFileURL, url.standardizedFileURL)
        XCTAssertTrue(model.isReadOnly)
        XCTAssertNotNil(content.readOnlyBanner)
        document.close()
    }

    /// While Save As UTF-8 runs, its button is off (a second click visibly
    /// does nothing) and so are Reload, Treat As and Reopen with Encoding;
    /// closing the document cancels it, writing nothing and saying nothing.
    func testWhileSavingTheButtonAndRereadsAreOffAndClosingCancels() async throws {
        let (url, _) = try utf16File("slow.csv", "id\tname\r\n1\tX\r\n")
        let (document, model, controller) = try open(url)
        let content = controller.content
        let button = try XCTUnwrap(content.readOnlyBanner?.button)
        let reload = NSMenuItem(title: "", action: #selector(DocumentViewController.reloadFromDisk(_:)), keyEquivalent: "")
        let treatAs = NSMenuItem(title: "", action: #selector(DocumentViewController.treatAsDelimiter(_:)), keyEquivalent: "")
        let reopen = NSMenuItem(title: "", action: #selector(DocumentViewController.reopenWithEncoding(_:)), keyEquivalent: "")
        reopen.representedObject = EncodingBox(model.interpretation.encoding)
        let items = [reload, treatAs, reopen]
        XCTAssertTrue(button.isEnabled)
        XCTAssertEqual(items.map(content.validateMenuItem), [true, true, true])

        content.chooseUTF8Destination = { _, _, _, done in done(self.directory.appending(path: "slow (UTF-8).csv")) }
        content.onSaveAsUTF8 = { _, _ in
            // A save that runs until it is cancelled, as the core's does.
            while !Task.isCancelled { try? await Task.sleep(for: .milliseconds(5)) }
            throw SaveFailure.Cancelled
        }
        var shown: NSAlert?
        content.showAlert = { alert, _ in shown = alert }
        button.performClick(nil)
        let task = try XCTUnwrap(content.savingAsUTF8)
        XCTAssertFalse(button.isEnabled)
        XCTAssertEqual(items.map(content.validateMenuItem), [false, false, false])
        content.saveAsUTF8(nil) // refused: still the same save
        XCTAssertTrue(content.savingAsUTF8 == task)

        document.close()
        await task.value
        XCTAssertNil(content.savingAsUTF8)
        XCTAssertNil(shown, "a cancelled save says nothing")
    }

    /// While Save As UTF-8 runs, editing and undo are off; an edit made all
    /// the same keeps the window on the UTF-16 file, with its edits and
    /// their history, and says so (task 2.5.2 review). The copy is saved.
    func testAnEditDuringSaveAsUTF8KeepsTheWindowOnTheFile() async throws {
        let (url, _) = try utf16File("edited.csv", "id\tname\r\n1\tZoë\r\n2\tAda\r\n")
        let (document, model, controller) = try open(url)
        let content = controller.content
        let copy = directory.appending(path: "edited (UTF-8).csv")
        content.chooseUTF8Destination = { _, _, _, done in done(copy) }
        var shown: [NSAlert] = []
        content.showAlert = { alert, _ in shown.append(alert) }
        let cell = CellPosition(row: 0, column: 1)
        guard case .edited = model.setCell(.cell(cell), to: "Zoe") else { return XCTFail("not edited") }
        let undo = document.history.undoManager
        XCTAssertTrue(undo.canUndo)

        content.grid.activeCell = cell
        content.saveAsUTF8(nil)
        let saving = try XCTUnwrap(content.savingAsUTF8)
        XCTAssertTrue(content.isReplacingDocument)
        content.editActiveCell()
        XCTAssertFalse(content.cellEditor.isEditing)
        XCTAssertFalse(undo.canUndo)
        // An edit made all the same, behind the window's back.
        guard case .edited = model.setCell(.cell(CellPosition(row: 1, column: 1)), to: "Ada L") else { return XCTFail("not edited") }
        await saving.value

        XCTAssertTrue(FileManager.default.fileExists(atPath: copy.path(percentEncoded: false)), "the copy was saved")
        XCTAssertEqual(document.fileURL?.standardizedFileURL, url.standardizedFileURL, "still the UTF-16 file")
        XCTAssertEqual(model.url.standardizedFileURL, url.standardizedFileURL)
        XCTAssertTrue(model.isReadOnly)
        // The core reads the copy now: Save stays off, asking for Save As
        // UTF-8 (task 2.5.3a), until a Reload.
        XCTAssertEqual(document.readsUTF8Copy?.standardizedFileURL, copy.standardizedFileURL)
        XCTAssertFalse(document.canSave)
        XCTAssertTrue(document.isDocumentEdited)
        XCTAssertEqual(model.fullValue(.cell(cell)), "Zoe")
        XCTAssertEqual(model.fullValue(.cell(CellPosition(row: 1, column: 1))), "Ada L")
        XCTAssertEqual(document.history.journal.count, 2)
        XCTAssertEqual(undo.undoActionName, "Typing")
        XCTAssertEqual(shown.map(\.messageText), ["The UTF-8 copy was saved, but this window still shows “edited.csv”."])
        content.editActiveCell()
        XCTAssertTrue(content.cellEditor.isEditing)
        content.discardEditing()
        document.close()
    }

    /// During a Reload, Save As UTF-8 is off.
    func testSaveAsUTF8IsOffDuringAReload() async throws {
        let (url, _) = try utf16File("reloading.csv", "id\tname\r\n1\tX\r\n")
        let (document, _, controller) = try open(url)
        let content = controller.content
        let button = try XCTUnwrap(content.readOnlyBanner?.button)
        content.onReload = {
            while !Task.isCancelled { try? await Task.sleep(for: .milliseconds(5)) }
        }
        content.reloadFromDisk(nil)
        XCTAssertFalse(content.canSaveAsUTF8)
        XCTAssertFalse(button.isEnabled)
        let reloading = try XCTUnwrap(content.reloading)
        reloading.cancel()
        await reloading.value
        XCTAssertTrue(content.canSaveAsUTF8)
        XCTAssertTrue(button.isEnabled)
        document.close()
    }

    /// The core's English never shows, and each failure has words of its
    /// own.
    func testFailuresAreWordedWithoutTheCoresEnglish() throws {
        func detail(_ failure: SaveFailure) throws -> String {
            try XCTUnwrap(SaveText.saveAsUTF8Failure(failure, headerRows: 1)).detail
        }
        let io = try detail(.Io(step: "writing the new file", code: 28, message: "No space left on device (os error 28)"))
        XCTAssertFalse(io.contains("os error"), io)
        let generic = try detail(.Unavailable)
        for failure in [SaveFailure.Internal(message: "boom"), .DocumentFailed(message: "boom"), .ChangedElsewhere] {
            let text = try detail(failure)
            XCTAssertNotEqual(text, generic, "\(failure)")
            XCTAssertFalse(text.contains("boom"), text)
        }
        XCTAssertEqual(
            try detail(.Unconvertible(encoding: .windows1253, cells: [], more: false)),
            "Some of the file’s bytes aren’t Windows-1253 text, so they can’t be converted. Leal never replaces them."
        )
        XCTAssertNil(SaveText.saveAsUTF8Failure(.Cancelled, headerRows: 1))
    }

    /// The suggested name keeps the extension, and a file without one gets
    /// none.
    func testTheSuggestedNameSaysUTF8() {
        XCTAssertEqual(DocumentViewController.utf8CopyName(of: URL(filePath: "/a/people.tsv")), "people (UTF-8).tsv")
        XCTAssertEqual(DocumentViewController.utf8CopyName(of: URL(filePath: "/a/people")), "people (UTF-8)")
    }
}
