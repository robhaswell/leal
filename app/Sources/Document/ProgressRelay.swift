import Foundation
import LealFFI

/// Receives indexing progress from the core and hands it to the main
/// thread, coalesced. Rust calls `indexProgressed` on the index thread
/// after every 1 MiB chunk (about a hundred times in 50 ms for the
/// reference file, 1.3a notes); the main thread gets at most one report
/// every `interval`, always the latest, and the final one at once.
final class ProgressRelay: ProgressObserver, @unchecked Sendable {
    // @unchecked: `latest` and `scheduled` are guarded by `lock`; `deliver`
    // is only ever read on the main actor.

    /// The longest a report waits for the main thread.
    static let interval: Duration = .milliseconds(100)

    private let lock = NSLock()
    private var latest: IndexProgress?
    private var scheduled = false
    private let deliver: @MainActor @Sendable (IndexProgress) -> Void

    /// `deliver` runs on the main actor with the latest report.
    init(deliver: @escaping @MainActor @Sendable (IndexProgress) -> Void) {
        self.deliver = deliver
    }

    func indexProgressed(progress: IndexProgress) {
        let start = lock.withLock {
            latest = progress
            if scheduled { return false }
            scheduled = true
            return true
        }
        guard start else { return }
        Task { @MainActor in
            if !progress.complete {
                try? await Task.sleep(for: Self.interval)
            }
            let report = lock.withLock {
                scheduled = false
                return latest
            }
            if let report { deliver(report) }
        }
    }
}
