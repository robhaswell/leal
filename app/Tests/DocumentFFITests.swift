import Foundation
import LealFFI
import XCTest

/// The document bindings of task 1.3a, through the generated Swift: first
/// paint, rows, progress callbacks, the async wait on a job and cancelling
/// it. The grid and the `withTaskCancellationHandler` wrapper are task 1.6.
final class DocumentFFITests: XCTestCase {
    func testOpenDocumentGivesTheFirstScreenThenRows() async throws {
        let url = try temporaryFile(named: "people.csv", contents: "name,city\nAda,London\n\"Zoë\",\"Zürich\nCH\"\n")
        let scheduler = try Scheduler()
        XCTAssertGreaterThanOrEqual(scheduler.backgroundThreads(), 1)
        let observer = ProgressRecorder()
        let document = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: temporaryLocations(),
            scheduler: scheduler,
            options: OpenOptions(firstScreenRows: 10, maxChars: 100),
            observer: observer
        )

        let screen = try document.firstScreen()
        XCTAssertEqual(screen.rows.map { $0.map(\.text) }, [["name", "city"], ["Ada", "London"], ["Zoë", "Zürich\nCH"]])
        XCTAssertEqual(screen.interpretation.delimiter, .comma)
        XCTAssertEqual(screen.interpretation.encoding, .utf8)
        XCTAssertTrue(screen.interpretation.header)
        XCTAssertEqual(screen.estimatedRowCount, 3)

        // The jobs run on Rust's threads; awaiting them doesn't block one.
        try await document.indexJob().wait()
        try await document.reviewJob().wait()
        XCTAssertEqual(try document.rowCount(), 3)
        XCTAssertTrue(try document.progress().complete)
        XCTAssertEqual(try document.review(), ReviewResult(encodingSuggestion: nil, delimiterSuggestion: nil))
        XCTAssertEqual(observer.reports.last?.complete, true)

        let rows = try document.rows(start: 1, count: 10, maxChars: 3)
        XCTAssertEqual(rows.map { $0.map(\.text) }, [["Ada", "Lon"], ["Zoë", "Zür"]])
        XCTAssertEqual(rows[0].map(\.truncated), [false, true])

        let semicolon = try document.reinterpret(options: OpenOptions(delimiter: .semicolon))
        XCTAssertEqual(semicolon.generation, 1)
        XCTAssertEqual(semicolon.interpretation.delimiterSource, .user)
    }

    /// ADR-0005 decision 6: `cancel()` on the handle stops the Rust job, and
    /// the awaiting Swift task gets `JobFailure.Cancelled`.
    func testCancellingAJobEndsItsWait() async throws {
        let url = try temporaryFile(named: "big.csv", contents: String(repeating: "a,b\n1,2\n", count: 1 << 20))
        let scheduler = try Scheduler()
        // Hold background work, as while the user scrolls, so the review
        // is still waiting when it is cancelled.
        scheduler.setInteracting(interacting: true)
        let document = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: temporaryLocations(),
            scheduler: scheduler,
            options: OpenOptions(),
            observer: nil
        )
        let review = try document.reviewJob()
        review.cancel()
        do {
            try await review.wait()
            XCTFail("a cancelled job should throw")
        } catch JobFailure.Cancelled {
            XCTAssertTrue(review.isFinished())
        }
        scheduler.setInteracting(interacting: false)
        try await document.indexJob().wait()
    }

    /// DESIGN §3.9: after a panic in a document call, the document has
    /// failed, and every later call throws `DocumentFailed` instead of
    /// touching it.
    func testAPanicInADocumentCallFailsTheDocument() throws {
        let url = try temporaryFile(named: "a.csv", contents: "a,b\n1,2\n")
        let path = url.path(percentEncoded: false)
        let document = try openDocument(
            path: path,
            volume: VolumeInfo(),
            temp: temporaryLocations(),
            scheduler: Scheduler(),
            options: OpenOptions(),
            observer: nil
        )
        XCTAssertFalse(document.isFailed())
        let failed = LealError.DocumentFailed(path: path, message: "deliberate document panic")
        XCTAssertThrowsError(try document.debugPanic()) { error in
            XCTAssertEqual(error as? LealError, failed)
            XCTAssertEqual(
                OpenErrorText.describe(error),
                "Something went wrong inside Leal. Close the file and open it again."
            )
        }
        XCTAssertTrue(document.isFailed())
        XCTAssertThrowsError(try document.rows(start: 0, count: 5, maxChars: 10)) { error in
            XCTAssertEqual(error as? LealError, failed)
        }
        XCTAssertThrowsError(try document.rowCount()) { error in
            XCTAssertEqual(error as? LealError, failed)
        }
    }

    /// A panic in a background job reaches the awaiting task as
    /// `JobFailure.Panicked`, and fails the document it belongs to.
    func testAPanicInAJobFailsTheDocument() async throws {
        let url = try temporaryFile(named: "a.csv", contents: "a,b\n1,2\n")
        let scheduler = try Scheduler()
        let document = try openDocument(
            path: url.path(percentEncoded: false),
            volume: VolumeInfo(),
            temp: temporaryLocations(),
            scheduler: scheduler,
            options: OpenOptions(),
            observer: nil
        )
        let job = debugPanickingJob(scheduler: scheduler)
        document.debugWatch(job: job)
        do {
            try await job.wait()
            XCTFail("a panicking job should throw")
        } catch let failure as JobFailure {
            XCTAssertEqual(failure, JobFailure.Panicked(message: "deliberate job panic"))
            XCTAssertEqual(
                OpenErrorText.describe(failure),
                "Something went wrong inside Leal. Close the file and open it again."
            )
        }
        // The document is marked as the job finishes, on its thread.
        let deadline = Date().addingTimeInterval(10)
        while !document.isFailed(), Date() < deadline {
            try await Task.sleep(nanoseconds: 1_000_000)
        }
        XCTAssertTrue(document.isFailed())
        XCTAssertEqual(OpenErrorText.describe(JobFailure.Cancelled), "It was stopped.")
    }

    func testDocumentErrorsAreWorded() throws {
        let url = try temporaryFile(named: "bom.csv", contents: "\u{FEFF}a,b\n")
        let path = url.path(percentEncoded: false)
        XCTAssertThrowsError(
            try openDocument(
                path: path,
                volume: VolumeInfo(),
                temp: temporaryLocations(),
                scheduler: Scheduler(),
                options: OpenOptions(encoding: .utf16Le),
                observer: nil
            )
        ) { error in
            XCTAssertEqual(error as? LealError, LealError.EncodingDoesNotFit(path: path))
            XCTAssertEqual(OpenErrorText.describe(error), "That encoding doesn’t match the file’s byte order mark.")
        }
        XCTAssertEqual(
            OpenErrorText.describe(LealError.DriveDisconnected(path: path)),
            "The drive it’s on was disconnected."
        )
        XCTAssertEqual(
            OpenErrorText.describe(LealError.TooLarge(path: path, byteCount: 1 << 33)),
            "It’s 4 GB or larger, more than Leal can open."
        )
    }

    /// A new directory that is deleted after the test.
    private func temporaryDirectory() throws -> URL {
        let directory = FileManager.default.temporaryDirectory.appending(path: "leal-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        addTeardownBlock {
            try? FileManager.default.removeItem(at: directory)
        }
        return directory
    }

    private func temporaryFile(named name: String, contents: String) throws -> URL {
        let url = try temporaryDirectory().appending(path: name)
        try Data(contents.utf8).write(to: url)
        return url
    }

    private func temporaryLocations() throws -> TempLocations {
        let directory = try temporaryDirectory()
        return TempLocations(
            scratchDir: directory.appending(path: "scratch").path(percentEncoded: false),
            recordsDir: directory.appending(path: "records").path(percentEncoded: false)
        )
    }
}

/// Records progress reports. Rust calls it on the index's thread, so it
/// guards its state with a lock.
private final class ProgressRecorder: ProgressObserver, @unchecked Sendable {
    private let lock = NSLock()
    private var stored: [IndexProgress] = []

    func indexProgressed(progress: IndexProgress) {
        lock.withLock { stored.append(progress) }
    }

    var reports: [IndexProgress] {
        lock.withLock { stored }
    }
}
