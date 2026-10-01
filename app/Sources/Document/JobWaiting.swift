import Foundation
import LealFFI

extension Job {
    /// Waits for the job to finish, and stops it if the waiting Swift task
    /// is cancelled (ADR-0005 decision 6, DESIGN §3.9).
    ///
    /// UniFFI doesn't pass Swift task cancellation through to Rust, so
    /// cancelling the task alone would leave the job running. This calls
    /// the job's `cancel()` from the cancellation handler, which sets the
    /// flag the Rust job checks at its next chunk boundary; the wait then
    /// ends with `JobFailure.Cancelled`. A task cancelled before it gets
    /// here cancels the job at once. Every await on a long job in the app
    /// goes through this.
    ///
    /// - Throws: the `JobFailure` the job ended with.
    func finish() async throws {
        try await withTaskCancellationHandler {
            try await wait()
        } onCancel: {
            cancel()
        }
    }
}
