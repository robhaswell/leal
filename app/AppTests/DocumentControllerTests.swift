import AppKit
import LealFFI
import XCTest

@testable import Leal

/// Leal's `DocumentController` (task 2.0): every open through it opens the
/// core's document off the main thread first, then hands it to the
/// `CSVDocument` AppKit makes. These check the ways in (open, an open of a
/// file already open or being opened, restoring a window, a path through a
/// symbolic link) and a failed open's words.
@MainActor
final class DocumentControllerTests: XCTestCase {
    private var directory: URL!
    private var environment: DocumentEnvironment!
    private var savedEnvironment: (() throws -> DocumentEnvironment)?

    override func setUp() async throws {
        directory = FileManager.default.temporaryDirectory.appending(path: "leal-controller-\(UUID().uuidString)")
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
        for document in NSDocumentController.shared.documents {
            document.close()
        }
        CoreRelease.finish()
        try? FileManager.default.removeItem(at: directory)
    }

    /// Records each core open's thread, and opens the file normally.
    private final class Opens: @unchecked Sendable {
        // @unchecked: `threads` is only touched with `lock` held.
        private let lock = NSLock()
        private var threads: [Bool] = []

        func record() {
            let onMain = Thread.isMainThread
            lock.withLock { threads.append(onMain) }
        }

        var onMainThread: [Bool] { lock.withLock { threads } }
    }

    private func recordOpens() -> Opens {
        let opens = Opens()
        DocumentModel.openForTesting = { path, environment, options, observer in
            opens.record()
            return try openDocument(
                path: path,
                volume: VolumeInfo(),
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer
            )
        }
        return opens
    }

    private func file(_ name: String, in folder: URL? = nil, contents: String = "id,name\n1,a\n2,b\n") throws -> URL {
        let url = (folder ?? directory).appending(path: name)
        try Data(contents.utf8).write(to: url)
        return url
    }

    private var controller: DocumentController {
        NSDocumentController.shared as! DocumentController
    }

    /// `NSDocumentController.shared.openDocument`, awaited.
    private func open(_ url: URL, display: Bool = false) async -> (NSDocument?, Bool, (any Error)?) {
        await withCheckedContinuation { continuation in
            NSDocumentController.shared.openDocument(withContentsOf: url, display: display) { document, wasOpen, error in
                continuation.resume(returning: (document, wasOpen, error))
            }
        }
    }

    func testTheSharedControllerIsLeals() {
        XCTAssertTrue(NSDocumentController.shared is DocumentController)
    }

    /// The core's document is opened off the main thread, handed over, and
    /// the document's modification date is the one read in the background.
    func testAnOpenReadsTheFileOffTheMainThread() async throws {
        let opens = recordOpens()
        let url = try file("a.csv")
        let (document, wasOpen, error) = await open(url)
        XCTAssertNil(error)
        XCTAssertFalse(wasOpen)
        let csv = try XCTUnwrap(document as? CSVDocument)
        XCTAssertEqual(opens.onMainThread, [false])
        XCTAssertNotNil(csv.model)
        XCTAssertEqual(csv.fileURL, url)
        XCTAssertEqual(csv.fileType, "public.comma-separated-values-text")
        let modified = try FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))[.modificationDate] as? Date
        XCTAssertEqual(csv.fileModificationDate, modified)
        csv.close()
    }

    /// A file the core can't open (it can't be read): the open error's
    /// words, from the core's background open, and nothing is left waiting.
    /// (A missing file never gets that far: AppKit refuses it first.)
    func testAFailedBackgroundOpenIsWorded() async throws {
        let url = try file("locked.csv")
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: url.path(percentEncoded: false))
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path(percentEncoded: false)) }
        let (document, _, error) = await open(url)
        XCTAssertNil(document)
        let failure = try XCTUnwrap(error) as NSError
        // AppKit's alert: its own title, with Leal's reason as the
        // suggestion, and Leal's error underneath.
        XCTAssertTrue(failure.localizedDescription.contains("locked.csv"), failure.localizedDescription)
        XCTAssertEqual(failure.localizedRecoverySuggestion, OpenErrorText.describe(LealError.PermissionDenied(path: "", code: EACCES)))
        let leal = try XCTUnwrap(failure.userInfo[NSUnderlyingErrorKey] as? NSError)
        XCTAssertEqual(leal.localizedDescription, "Leal couldn’t open “locked.csv”.")
        XCTAssertNil(PendingOpens.take(url), "nothing left waiting")
        XCTAssertTrue(NSDocumentController.shared.documents.isEmpty)
    }

    /// Opening a file that is already open gives the same document, and
    /// doesn't open the file again.
    func testOpeningAnOpenFileGivesItsDocument() async throws {
        let opens = recordOpens()
        let url = try file("a.csv")
        let (first, _, _) = await open(url)
        let (second, wasOpen, error) = await open(url)
        XCTAssertNil(error)
        XCTAssertTrue(wasOpen)
        XCTAssertTrue(first === second)
        XCTAssertEqual(opens.onMainThread, [false], "opened once")
        first?.close()
    }

    /// Two opens of one file at once: one core open, one document, both
    /// told about it.
    func testTwoOpensOfOneFileAtOnceShareOneOpen() async throws {
        let opens = recordOpens()
        let url = try file("a.csv")
        // Both start before either finishes: the second asks while the
        // first's core open is still on the file queue.
        let one = Task { await self.open(url) }
        let two = Task { await self.open(url) }
        let (first, second) = (await one.value, await two.value)
        XCTAssertNil(first.2)
        XCTAssertNil(second.2)
        XCTAssertNotNil(first.0)
        XCTAssertTrue(first.0 === second.0)
        XCTAssertEqual(opens.onMainThread, [false], "one core open")
        XCTAssertEqual(NSDocumentController.shared.documents.count, 1)
        first.0?.close()
    }

    /// Restoring a window (`reopenDocument`) goes the same way.
    func testRestoringAWindowOpensOffTheMainThread() async throws {
        let opens = recordOpens()
        let url = try file("a.csv")
        let (document, _, error): (NSDocument?, Bool, (any Error)?) = await withCheckedContinuation { continuation in
            NSDocumentController.shared.reopenDocument(for: url, withContentsOf: url, display: false) { document, wasOpen, error in
                continuation.resume(returning: (document, wasOpen, error))
            }
        }
        XCTAssertNil(error)
        let csv = try XCTUnwrap(document as? CSVDocument)
        XCTAssertNotNil(csv.model)
        XCTAssertEqual(opens.onMainThread, [false])
        csv.close()
    }

    /// A path through a symbolic link: AppKit makes the document for the URL
    /// it was asked to open, as given, so the background open is found
    /// under that one key, and the main thread never opens the file itself.
    func testAPathThroughASymbolicLinkFindsItsOpen() async throws {
        let opens = recordOpens()
        let real = directory.appending(path: "real")
        try FileManager.default.createDirectory(at: real, withIntermediateDirectories: true)
        _ = try file("a.csv", in: real)
        let link = directory.appending(path: "link")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: real)
        let (document, _, error) = await open(link.appending(path: "a.csv"))
        XCTAssertNil(error)
        XCTAssertNotNil((document as? CSVDocument)?.model)
        XCTAssertEqual(opens.onMainThread, [false], "never opened on the main thread")
        document?.close()
    }

    /// The modification date is the one from before the core's snapshot
    /// (task 2.0 re-review): a change right after it is still a change by
    /// the time Leal saves. The open hook changes the file's date after
    /// opening it.
    func testTheModificationDateIsFromBeforeTheOpen() async throws {
        let url = try file("a.csv")
        let path = url.path(percentEncoded: false)
        let before = try XCTUnwrap(try FileManager.default.attributesOfItem(atPath: path)[.modificationDate] as? Date)
        let later = before.addingTimeInterval(3600)
        DocumentModel.openForTesting = { path, environment, options, observer in
            let opened = try openDocument(
                path: path,
                volume: VolumeInfo(),
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer
            )
            try FileManager.default.setAttributes([.modificationDate: later], ofItemAtPath: path)
            return opened
        }
        let (document, _, error) = await open(url)
        XCTAssertNil(error)
        let date = try XCTUnwrap(document?.fileModificationDate)
        XCTAssertEqual(date.timeIntervalSince1970, before.timeIntervalSince1970, accuracy: 0.001)
        document?.close()
    }

    /// A file's type comes from its name, in any case, without a look at the
    /// file (it needn't exist).
    func testTheTypeComesFromTheName() throws {
        XCTAssertEqual(try controller.typeForContents(of: URL(filePath: "/nowhere/x.CSV")), "public.comma-separated-values-text")
        XCTAssertEqual(try controller.typeForContents(of: URL(filePath: "/nowhere/y.TSV")), "public.tab-separated-values-text")
        XCTAssertEqual(try controller.typeForContents(of: URL(filePath: "/nowhere/z.tsv")), "public.tab-separated-values-text")
    }

    /// A TSV opens through the controller as a TSV.
    func testATSVOpensThroughTheController() async throws {
        let opens = recordOpens()
        let url = try file("a.tsv", contents: "id\tname\n1\ta\n2\tb\n")
        let (document, _, error) = await open(url)
        XCTAssertNil(error)
        let csv = try XCTUnwrap(document as? CSVDocument)
        XCTAssertEqual(csv.fileType, "public.tab-separated-values-text")
        XCTAssertEqual(csv.model?.interpretation.delimiter, .tab)
        XCTAssertEqual(opens.onMainThread, [false])
        csv.close()
    }

    /// Two opens at once of a file that can't be opened: every caller gets
    /// the error, and nothing is left waiting.
    func testEveryWaiterGetsASharedOpensError() async throws {
        let url = try file("locked.csv")
        let path = url.path(percentEncoded: false)
        try FileManager.default.setAttributes([.posixPermissions: 0o000], ofItemAtPath: path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: path) }
        let one = Task { await self.open(url) }
        let two = Task { await self.open(url) }
        let (first, second) = (await one.value, await two.value)
        XCTAssertNil(first.0)
        XCTAssertNil(second.0)
        XCTAssertNotNil(first.2)
        XCTAssertNotNil(second.2)
        XCTAssertNil(PendingOpens.take(url))
        XCTAssertTrue(NSDocumentController.shared.documents.isEmpty)
    }

    /// A caller that asks for a window while an open without one is under
    /// way gets one.
    func testAWaiterThatWantsAWindowGetsOne() async throws {
        _ = recordOpens()
        let url = try file("a.csv")
        let one = Task { await self.open(url, display: false) }
        let two = Task { await self.open(url, display: true) }
        let (first, second) = (await one.value, await two.value)
        let document = try XCTUnwrap(first.0)
        XCTAssertTrue(document === second.0)
        XCTAssertFalse(document.windowControllers.isEmpty, "the second caller wanted a window")
        XCTAssertEqual(document.windowControllers.first?.window?.isVisible, true)
        document.close()
    }

    /// An open the controller passes straight on (no environment) isn't
    /// called a missed open (no fault), and is then forgotten.
    func testASkippedOpenIsKnownAsSkipped() throws {
        let url = directory.appending(path: "skipped.csv")
        PendingOpens.skip(url)
        XCTAssertTrue(PendingOpens.wasSkipped(url))
        XCTAssertFalse(PendingOpens.wasSkipped(url), "only once")
    }
}
