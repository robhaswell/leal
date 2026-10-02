import Foundation

/// Where Leal touches the user's files off the main thread (task 2.0): the
/// core's opens and Reloads (first paint reads the file), and every look at
/// a file that may be on a network share (`stat`, `checkOriginal`).
///
/// A queue of its own, not Swift's `Task.detached`: a share that stops
/// answering blocks a thread for as long as it likes, and Swift's
/// cooperative pool has only a thread per core, which a few hung shares
/// would use up, stalling every `async` function in the app. Dispatch makes
/// more threads when these block (ADR-0009).
enum FileWork {
    static let queue = DispatchQueue(
        label: "io.github.robhaswell.leal.file-work",
        qos: .userInitiated,
        attributes: .concurrent
    )

    /// Runs `work` on `queue`, at `qos`, and waits for it without holding a
    /// thread of Swift's pool.
    static func run<T: Sendable>(qos: DispatchQoS = .userInitiated, _ work: @escaping @Sendable () throws -> T) async throws -> T {
        try await withCheckedThrowingContinuation { continuation in
            queue.async(qos: qos) {
                continuation.resume(with: Result { try work() })
            }
        }
    }

    /// `run`, for work that can't fail.
    static func run<T: Sendable>(qos: DispatchQoS = .userInitiated, _ work: @escaping @Sendable () -> T) async -> T {
        await withCheckedContinuation { continuation in
            queue.async(qos: qos) {
                continuation.resume(returning: work())
            }
        }
    }
}
