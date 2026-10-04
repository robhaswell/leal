import Foundation

/// A coordinated write that replaces a file (`NSFileCoordinator`,
/// `.forReplacing`), held while an async save runs (task 2.5.3a,
/// ADR-0012's consequences): other apps, iCloud Drive and File Provider
/// folders get coordinated notice of Leal's save, and the presenter passed
/// in (the document) isn't told about its own write.
///
/// The coordinator calls its accessor on a queue of its own once every
/// other presenter has let go, and the access lasts until the accessor
/// returns. So the accessor hands the file's URL to the waiting task and
/// then waits, on that queue's thread (never the main thread, never Swift's
/// pool), until `end()`.
final class CoordinatedWrite: @unchecked Sendable {
    /// The coordinator's accessors run here, each on a thread of its own.
    private static let queue: OperationQueue = {
        let queue = OperationQueue()
        queue.name = "io.github.robhaswell.leal.coordinated-write"
        queue.qualityOfService = .userInitiated
        return queue
    }()

    /// Where to write: the file's URL as the coordinator gives it (it
    /// follows a move made while waiting for access).
    let url: URL
    private let ended = DispatchSemaphore(value: 0)

    private init(url: URL) {
        self.url = url
    }

    /// Waits, without blocking the caller's thread, for a coordinated
    /// write that replaces `url`; `end()` ends it.
    ///
    /// - Throws: the coordinator's error, if it couldn't give access.
    static func begin(replacing url: URL, presenter: (any NSFilePresenter)?) async throws -> CoordinatedWrite {
        let coordinator = NSFileCoordinator(filePresenter: presenter)
        let intent = NSFileAccessIntent.writingIntent(with: url, options: .forReplacing)
        return try await withCheckedThrowingContinuation { continuation in
            coordinator.coordinate(with: [intent], queue: queue) { error in
                if let error {
                    continuation.resume(throwing: error)
                    return
                }
                let write = CoordinatedWrite(url: intent.url)
                continuation.resume(returning: write)
                // The access lasts while this accessor runs.
                write.ended.wait()
            }
        }
    }

    /// Ends the coordinated write. Call it exactly once.
    func end() {
        ended.signal()
    }
}
