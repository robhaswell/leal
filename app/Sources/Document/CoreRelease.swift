import Foundation

/// Lets go of the core's objects off the main thread (phase 1 review,
/// app-10).
///
/// Releasing the last reference to a core document does real work: it
/// stops the document's file watcher and waits for the watcher's thread to
/// end, cancels its jobs, and deletes its snapshot. Done on the main
/// thread (closing a window, a Reload, a new find query), that wait was on
/// a thread of lower quality of service than the main thread's, which the
/// Thread Performance Checker reports as a priority inversion, and which
/// can stall the window behind a slow volume. The object is handed to a
/// utility queue instead, which drops the last reference.
enum CoreRelease {
    /// The queue the core's objects are released on, one at a time.
    private static let queue = DispatchQueue(label: "io.github.robhaswell.leal.core-release", qos: .utility)

    /// Takes the object out of `reference` (leaving `nil`) and releases it
    /// on a utility queue. Whoever else still holds it (a background
    /// read, a promised copy) lets go of it on their own thread.
    static func later<Object: AnyObject & Sendable>(_ reference: inout Object?) {
        guard reference != nil else { return }
        // `take()` moves the reference into the holder without a copy, so
        // nothing on this thread holds the object once this returns.
        let holder = Holder(reference.take())
        queue.async {
            holder.release()
        }
    }

    /// Waits until everything handed over has been released, so that the
    /// snapshots of documents closed just before Leal quits are deleted
    /// (DESIGN §3.1). `sync` lends the queue the caller's quality of
    /// service while it waits.
    ///
    /// Each wait gives up after about a second (task 2.0a review): a hung
    /// release must not stop Leal from quitting.
    static func finish() {
        // A grid's read ahead may hold a closed document until it ends, and
        // then hands it here (`DocumentModel.backgroundTileReader`).
        CellTileCache.finishReadsAhead(timeout: .now() + 1)
        wait(for: queue, timeout: .now() + 1)
    }

    /// Waits for what `queue` has queued so far, lending it the caller's
    /// quality of service, for at most until `timeout`.
    static func wait(for queue: DispatchQueue, timeout: DispatchTime) {
        let done = DispatchSemaphore(value: 0)
        queue.async(qos: .userInitiated, flags: .enforceQoS) { done.signal() }
        _ = done.wait(timeout: timeout)
    }

    /// Holds the object until the queue lets go of it.
    private final class Holder: @unchecked Sendable {
        // @unchecked: `object` is set once, on the caller's thread, and
        // cleared once, on the queue, after `async` has ordered the two.
        private var object: (any AnyObject & Sendable)?

        init(_ object: (any AnyObject & Sendable)?) {
            self.object = object
        }

        func release() {
            object = nil
        }
    }
}
