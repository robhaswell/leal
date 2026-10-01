import Foundation
import LealFFI

/// Receives the core's reports about the user's file (task 1.9: changed,
/// moved, deleted, or its volume gone) on the core's watching thread, and
/// hands each to the main actor. They are rare (one per change of state),
/// so none are coalesced.
final class OriginalRelay: OriginalObserver, Sendable {
    private let deliver: @MainActor @Sendable (OriginalStatus) -> Void

    /// `deliver` runs on the main actor with each report.
    init(deliver: @escaping @MainActor @Sendable (OriginalStatus) -> Void) {
        self.deliver = deliver
    }

    func originalChanged(status: OriginalStatus) {
        let deliver = deliver
        Task { @MainActor in deliver(status) }
    }
}
