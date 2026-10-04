import Foundation
import os

/// The app's `os_signpost`s (DESIGN §3.10, "Measuring it"), beside the
/// core's ("First paint", "Index", "Review", …): the same subsystem and the
/// Points of Interest category, so Instruments shows them all in one lane,
/// and `just perf` reads them from the shipped app with `log stream`.
///
/// - "Launched", an event when the app has finished launching, with the
///   milliseconds since the process started: the DESIGN §1 launch budget.
/// - "Open to first rows", an interval from `CSVDocument` starting to read
///   a file to the grid's first draw with rows in it: the open budget.
/// - "Cell edit to screen", an interval from an edit's commit to the
///   transaction that draws it: the edit budget (task 2.5.1).
///
/// Signposts cost a check of a flag unless something is recording them.
enum Signposts {
    static let signposter = OSSignposter(subsystem: "io.github.robhaswell.leal", category: .pointsOfInterest)

    /// The app has finished launching.
    static func launched() {
        let milliseconds = processAge() * 1000
        signposter.emitEvent("Launched", "\(milliseconds, format: .fixed(precision: 3), privacy: .public) ms after the process started")
    }

    /// The start of an open; `firstRows(_:)` ends it.
    static func opening() -> OSSignpostIntervalState {
        signposter.beginInterval("Open to first rows", id: signposter.makeSignpostID())
    }

    static func firstRows(_ state: OSSignpostIntervalState) {
        signposter.endInterval("Open to first rows", state)
    }

    /// The start of an edit's commit (task 2.5.1); `editOnScreen(_:)`
    /// ends it once the transaction with the edited cell's drawing is
    /// committed: "Cell edit to screen", DESIGN §1's < 16 ms.
    static func editCommitted() -> OSSignpostIntervalState {
        signposter.beginInterval("Cell edit to screen", id: signposter.makeSignpostID())
    }

    static func editOnScreen(_ state: OSSignpostIntervalState) {
        signposter.endInterval("Cell edit to screen", state)
    }

    /// Seconds since this process started, from the kernel's record of it.
    private static func processAge() -> Double {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        guard sysctl(&mib, 4, &info, &size, nil, 0) == 0 else { return -1 }
        let start = info.kp_proc.p_un.__p_starttime
        let started = Double(start.tv_sec) + Double(start.tv_usec) / 1e6
        return Date().timeIntervalSince1970 - started
    }
}
