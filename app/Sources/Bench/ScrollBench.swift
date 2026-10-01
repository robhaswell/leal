import AppKit
import QuartzCore

// Only in the builds `just bench-scroll` and `just snapshot` make
// (`LEAL_BENCH`): the shipped app has no scripted runs (CI checks with nm).
#if LEAL_BENCH

/// The scroll benchmark (`-LealBenchScroll`), the spike's method (docs/tasks/
/// 0.4.md) on the real grid and the real core: a display link fires once
/// per refresh, records the time since the last frame, and moves the
/// scroll position, like a trackpad fling. If the main thread takes more
/// than a refresh to lay out and draw, callbacks are skipped and the next
/// interval is a multiple of the refresh period.
///
/// Stages: vertical flings, horizontal flings to the far right, vertical
/// flings there, back to the left, a jump to the last row and back to the
/// top. Each frame also tells the scheduler the user is scrolling, as a
/// real scroll does. Results: frame intervals, main-thread busy time (wall
/// time less the time the run loop slept) and main-thread CPU time per
/// stage, first paint, memory before and after.
@MainActor
final class ScrollBench: NSObject {
    private let document: CSVDocument
    private let content: DocumentViewController
    private let speed: Double
    private let flingRows: Int
    private var result: [String: Any] = [:]
    private var finish: (([String: Any]) -> Void)?

    private var link: CADisplayLink?
    private var lastTimestamp: CFTimeInterval = 0
    private var lastWall: CFTimeInterval = 0
    private var stages: [(name: String, step: (Double) -> Bool)] = []
    private var stageStart: CFTimeInterval = 0
    private var frameInStage = 0
    private var recording: String?
    private var intervals: [String: [Double]] = [:]
    private var busy: [String: [Double]] = [:]
    /// The main thread's CPU time per frame: unlike busy time, it doesn't
    /// grow when other processes compete for the cores.
    private var cpu: [String: [Double]] = [:]
    private var lastCPU: UInt64 = 0
    /// The grid area drawn per frame, in screens.
    private var drawn: [String: [Double]] = [:]
    private var lastDrawn: CGFloat = 0
    private var refresh: [Double] = []
    private var peakFootprint = 0.0
    private var peakHeap = 0.0
    private var ticks = 0
    private var stalls = 0
    private var sleepStart: CFTimeInterval = 0
    private var slept: CFTimeInterval = 0
    private var observers: [CFRunLoopObserver] = []

    init(document: CSVDocument, content: DocumentViewController, profile: String) {
        self.document = document
        self.content = content
        // The spike's profiles: fast is 60,000 pt/s flings over 50,000
        // rows; moderate is 15,000 pt/s over 10,000.
        (speed, flingRows) = profile == "moderate" ? (15_000, 10_000) : (60_000, 50_000)
        super.init()
        result["profile"] = profile
    }

    /// The spike's measuring conditions (docs/tasks/0.4.md): no App Nap or
    /// timer coalescing, as while the user scrolls with a trackpad (scripted
    /// scrolling sends no input events, so macOS would otherwise treat the
    /// app as idle and slow its cores), and a window nothing covers.
    private var activity: NSObjectProtocol?
    private let defaults = UserDefaults.standard

    func start(finish: @escaping ([String: Any]) -> Void) {
        self.finish = finish
        activity = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiated, .latencyCritical],
            reason: "Scroll benchmark"
        )
        content.view.window?.level = .floating
        guard let view = content.view.window?.contentView else { return }
        let link = view.displayLink(target: self, selector: #selector(tick(_:)))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 120, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        self.link = link
        // Busy time = a frame's wall time less the time the main run loop
        // slept in it: layout, drawing and the Core Animation commit.
        let sleep = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.beforeWaiting.rawValue, true, CFIndex.max) { [weak self] _, _ in
            MainActor.assumeIsolated { self?.sleepStart = CACurrentMediaTime() }
        }
        let wake = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.afterWaiting.rawValue, true, CFIndex.min) { [weak self] _, _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                if self.sleepStart > 0 { self.slept += CACurrentMediaTime() - self.sleepStart }
                self.sleepStart = 0
            }
        }
        for observer in [wake, sleep].compactMap({ $0 }) {
            CFRunLoopAddObserver(CFRunLoopGetMain(), observer, .commonModes)
            observers.append(observer)
        }
        describeEnvironment()
        buildStages()
        stageStart = CACurrentMediaTime()
    }

    private var clip: NSClipView { content.grid.scrollView.contentView }
    private var maxY: CGFloat { max(0, content.grid.gridView.frame.height - clip.bounds.height) }
    private var maxX: CGFloat { max(0, content.grid.gridView.frame.width - clip.bounds.width) }

    private func scroll(x: CGFloat? = nil, y: CGFloat? = nil) {
        let origin = clip.bounds.origin
        clip.scroll(to: NSPoint(x: min(max(0, x ?? origin.x), maxX), y: min(max(0, y ?? origin.y), maxY)))
        content.grid.scrollView.reflectScrolledClipView(clip)
        // As a real scroll event does (DESIGN §3.10 rule 3).
        content.grid.onUserInput?()
    }

    private func describeEnvironment() {
        let window = content.view.window
        result["hwModel"] = Self.sysctl("hw.model")
        result["cpu"] = Self.sysctl("machdep.cpu.brand_string")
        result["osVersion"] = ProcessInfo.processInfo.operatingSystemVersionString
        result["screenMaxFPS"] = window?.screen?.maximumFramesPerSecond ?? 0
        result["backingScale"] = Double(window?.backingScaleFactor ?? 0)
        result["windowSize"] = window.map { "\(Int($0.frame.width))x\(Int($0.frame.height))" } ?? "?"
        result["file"] = document.fileURL?.lastPathComponent ?? "?"
        result["columns"] = content.model.columnCount
        result["loadAvgAtStart"] = Self.loadAverage()
    }

    private func buildStages() {
        stages.append(("indexed", { [unowned self] _ in
            guard content.model.isIndexComplete else { return false }
            result["rows"] = content.model.rowCount
            if let opened = document.openStarted, let drawn = content.grid.gridView.firstDrawTime {
                result["openToFirstRowsMs"] = (drawn - opened) * 1000
                result["launchToFirstRowsMs"] = (drawn - Self.launchTime) * 1000
            }
            result["indexedAfterMs"] = (CACurrentMediaTime() - (document.openStarted ?? 0)) * 1000
            return true
        }))
        stages.append(("afterLoad", settle(1.0) { [unowned self] in
            result["footprintAfterLoadMB"] = Memory.footprintMB()
            result["heapAfterLoadMB"] = Memory.heapMB()
            result["refreshMsMedian"] = Self.median(refresh)
        }))
        let half = flingRows / 2
        stages.append(("vertical", verticalFlings("vertical", rows: half)))
        if defaults.bool(forKey: "LealBenchVerticalOnly") {
            stages.append(("settled", settle(0.1) { [unowned self] in done() }))
            return
        }
        stages.append(("horizontalRight", horizontal("horizontal", right: true)))
        stages.append(("verticalWide", verticalFlings("verticalWide", rows: half)))
        stages.append(("horizontalLeft", horizontal("horizontal", right: false)))
        stages.append(("jumpEnd", { [unowned self] _ in
            if frameInStage == 0 {
                scroll(y: maxY)
                recording = "jumpEnd"
            }
            return frameInStage >= 30
        }))
        stages.append(("jumpTop", { [unowned self] _ in
            if frameInStage == 0 {
                scroll(y: 0)
                recording = "jumpTop"
            }
            return frameInStage >= 30
        }))
        stages.append(("afterScroll", settle(1.0) { [unowned self] in
            result["footprintAfterScrollMB"] = Memory.footprintMB()
            result["heapAfterScrollMB"] = Memory.heapMB()
            result["footprintPeakMB"] = max(peakFootprint, Memory.footprintMB())
            result["heapPeakMB"] = max(peakHeap, Memory.heapMB())
        }))
        stages.append(("settled", settle(4.0) { [unowned self] in
            result["footprintSettledMB"] = Memory.footprintMB()
            result["heapSettledMB"] = Memory.heapMB()
            done()
        }))
    }

    private func settle(_ seconds: Double, then: @escaping () -> Void) -> (Double) -> Bool {
        { [unowned self] _ in
            guard CACurrentMediaTime() - stageStart >= seconds else { return false }
            then()
            return true
        }
    }

    /// Flings that start at `speed` and decay, like a trackpad's momentum,
    /// over `rows` rows in all.
    private func verticalFlings(_ name: String, rows: Int) -> (Double) -> Bool {
        var fling = Fling(start: speed, decay: 0.5, minimum: speed / 30)
        var travelled: CGFloat = 0
        let target = CGFloat(rows) * GridMetrics.rowHeight
        return { [unowned self] dt in
            let distance = CGFloat(fling.step(dt))
            travelled += distance
            scroll(y: clip.bounds.origin.y + distance)
            recording = name
            return travelled >= target || clip.bounds.origin.y >= maxY
        }
    }

    private func horizontal(_ name: String, right: Bool) -> (Double) -> Bool {
        let start = speed / 4
        var fling = Fling(start: right ? start : -start, decay: 0.4, minimum: start / 20)
        return { [unowned self] dt in
            scroll(x: clip.bounds.origin.x + CGFloat(fling.step(dt)))
            recording = name
            let x = clip.bounds.origin.x
            let done = right ? x >= maxX - 0.5 : x <= 0.5
            if done {
                // End exactly at the edge, as a trackpad scroll does.
                scroll(x: right ? maxX : 0)
            }
            return done
        }
    }

    @objc private func tick(_ link: CADisplayLink) {
        let timestamp = link.timestamp
        let wall = CACurrentMediaTime()
        let cpuNow = clock_gettime_nsec_np(CLOCK_THREAD_CPUTIME_ID)
        refresh.append((link.targetTimestamp - link.timestamp) * 1000)
        var dt = 1.0 / 120
        if lastTimestamp > 0 {
            dt = min(0.05, timestamp - lastTimestamp)
            // A gap this long is the display going to sleep, not a slow
            // frame; the run is flagged.
            if timestamp - lastTimestamp > 0.5 { stalls += 1 }
            if let phase = recording {
                intervals[phase, default: []].append((timestamp - lastTimestamp) * 1000)
                busy[phase, default: []].append(max(0, (wall - lastWall) - slept) * 1000)
                cpu[phase, default: []].append(Double(cpuNow - lastCPU) / 1e6)
                let screen = max(1, content.grid.scrollView.contentSize.width * content.grid.scrollView.contentSize.height)
                drawn[phase, default: []].append(Double((content.grid.gridView.drawnArea - lastDrawn) / screen))
            }
        }
        recording = nil
        slept = 0
        lastTimestamp = timestamp
        lastWall = wall
        lastCPU = cpuNow
        lastDrawn = content.grid.gridView.drawnArea
        ticks += 1
        if ticks % 15 == 0 {
            peakFootprint = max(peakFootprint, Memory.footprintMB())
            peakHeap = max(peakHeap, Memory.heapMB())
        }
        guard let stage = stages.first else { return }
        if stage.step(dt) {
            stages.removeFirst()
            stageStart = CACurrentMediaTime()
            frameInStage = 0
        } else {
            frameInStage += 1
        }
    }

    private func done() {
        link?.invalidate()
        if let activity { ProcessInfo.processInfo.endActivity(activity) }
        for observer in observers {
            CFRunLoopRemoveObserver(CFRunLoopGetMain(), observer, .commonModes)
        }
        let refreshMs = (result["refreshMsMedian"] as? Double) ?? 1000.0 / 120
        var scrollIntervals: [Double] = []
        var scrollBusy: [Double] = []
        var scrollCPU: [Double] = []
        var phases: [String: Any] = [:]
        for (phase, values) in intervals {
            var stats = Self.stats(values, refresh: refreshMs, busy: busy[phase] ?? [], cpu: cpu[phase] ?? [])
            let area = drawn[phase] ?? []
            stats["drawnScreensPerFrame"] = area.isEmpty ? 0 : area.reduce(0, +) / Double(area.count)
            phases[phase] = stats
            if phase != "jumpEnd", phase != "jumpTop" {
                scrollIntervals += values
                scrollBusy += busy[phase] ?? []
                scrollCPU += cpu[phase] ?? []
            }
        }
        result["phases"] = phases
        result["scroll"] = Self.stats(scrollIntervals, refresh: refreshMs, busy: scrollBusy, cpu: scrollCPU)
        result["jumpEndMs"] = intervals["jumpEnd"]?.first ?? 0
        let grid = content.grid
        result["cellsDrawn"] = grid.gridView.cellsDrawn
        result["gridDraws"] = grid.gridView.draws
        result["gridDrawnScreens"] = Double(grid.gridView.drawnArea / max(1, grid.scrollView.contentSize.width * grid.scrollView.contentSize.height))
        result["headerDraws"] = grid.headerView.draws
        result["gutterDraws"] = grid.gutterView.draws
        result["displayLinkStalls"] = stalls
        result["loadAvgAtEnd"] = Self.loadAverage()
        result["windowVisible"] = content.view.window?.occlusionState.contains(.visible) ?? false
        finish?(result)
    }

    /// Percentiles of frame intervals, late frames (more than 1.5 refreshes
    /// apart: at least one refresh missed) and busy time.
    private static func stats(_ values: [Double], refresh: Double, busy: [Double], cpu: [Double]) -> [String: Any] {
        func percentile(_ sorted: [Double], _ p: Double) -> Double {
            sorted.isEmpty ? 0 : sorted[min(sorted.count - 1, Int(Double(sorted.count - 1) * p + 0.5))]
        }
        let sorted = values.sorted()
        let busySorted = busy.sorted()
        let cpuSorted = cpu.sorted()
        return [
            "frames": values.count,
            "p50": percentile(sorted, 0.5),
            "p95": percentile(sorted, 0.95),
            "p99": percentile(sorted, 0.99),
            "max": sorted.last ?? 0,
            "late": values.filter { $0 > refresh * 1.5 }.count,
            "late2": values.filter { $0 > refresh * 2.5 }.count,
            "busyP50": percentile(busySorted, 0.5),
            "busyP99": percentile(busySorted, 0.99),
            "busyMax": busySorted.last ?? 0,
            "cpuP50": percentile(cpuSorted, 0.5),
            "cpuP99": percentile(cpuSorted, 0.99),
            "cpuMax": cpuSorted.last ?? 0,
        ]
    }

    private static func median(_ values: [Double]) -> Double {
        let sorted = values.sorted()
        return sorted.isEmpty ? 0 : sorted[sorted.count / 2]
    }

    private static func loadAverage() -> Double {
        var loads = [Double](repeating: 0, count: 3)
        return getloadavg(&loads, 3) > 0 ? loads[0] : -1
    }

    private static func sysctl(_ name: String) -> String {
        var size = 0
        sysctlbyname(name, nil, &size, nil, 0)
        var buffer = [CChar](repeating: 0, count: max(1, size))
        sysctlbyname(name, &buffer, &size, nil, 0)
        return String(decoding: buffer.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    }

    /// When the process started, in `CACurrentMediaTime` terms.
    private static let launchTime: CFTimeInterval = {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        Darwin.sysctl(&mib, 4, &info, &size, nil, 0)
        let start = info.kp_proc.p_un.__p_starttime
        let started = Double(start.tv_sec) + Double(start.tv_usec) / 1e6
        return CACurrentMediaTime() - (Date().timeIntervalSince1970 - started)
    }()
}

/// A momentum scroll: it starts at `start` points a second and decays
/// exponentially, like a trackpad fling; when it slows below `minimum` a
/// new fling starts.
private struct Fling {
    let start: Double
    let decay: Double
    let minimum: Double
    var velocity: Double = 0

    mutating func step(_ dt: Double) -> Double {
        if abs(velocity) < minimum { velocity = start }
        let distance = velocity * dt
        velocity *= exp(-dt / decay)
        return distance
    }
}

/// The app's memory, as `footprint(1)` and Instruments count it.
enum Memory {
    /// Everything macOS charges to the app (`phys_footprint`), graphics
    /// surfaces included.
    static func footprintMB() -> Double {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
        let result = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
            }
        }
        return result == KERN_SUCCESS ? Double(info.phys_footprint) / 1_048_576 : -1
    }

    /// Bytes in use in every malloc zone: Leal's own heap, without mapped
    /// file pages or graphics surfaces (the DESIGN §1 budget is 40 MB).
    static func heapMB() -> Double {
        var stats = malloc_statistics_t()
        malloc_zone_statistics(nil, &stats)
        return Double(stats.size_in_use) / 1_048_576
    }
}

#endif
