import AppKit
import QuartzCore

// Scripted scrolling and measurement. A display link (NSView.displayLink,
// macOS 14+) fires once per display refresh; each callback records the
// interval since the previous frame and advances the scroll position. If the
// main thread takes longer than one refresh to lay out and draw, callbacks
// are skipped and the next interval is a multiple of the refresh period.

func footprintMB() -> Double {
    var info = task_vm_info_data_t()
    var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
    let kr = withUnsafeMutablePointer(to: &info) {
        $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) {
            task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count)
        }
    }
    return kr == KERN_SUCCESS ? Double(info.phys_footprint) / 1_048_576 : -1
}

/// Bytes in use across all malloc zones: the app's own heap, without the
/// graphics surfaces that phys_footprint also counts.
func heapMB() -> Double {
    var stats = malloc_statistics_t()
    malloc_zone_statistics(nil, &stats)
    return Double(stats.size_in_use) / 1_048_576
}

func processStartTime() -> Double {
    var kp = kinfo_proc()
    var size = MemoryLayout<kinfo_proc>.stride
    var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
    sysctl(&mib, 4, &kp, &size, nil, 0)
    let tv = kp.kp_proc.p_un.__p_starttime
    return Double(tv.tv_sec) + Double(tv.tv_usec) / 1e6
}

func sysctlString(_ name: String) -> String {
    var size = 0
    sysctlbyname(name, nil, &size, nil, 0)
    var buf = [CChar](repeating: 0, count: size)
    sysctlbyname(name, &buf, &size, nil, 0)
    return String(cString: buf)
}

/// Momentum scroll: starts at v0 and decays exponentially, like a trackpad
/// fling; when it slows below vmin a new fling starts.
struct Fling {
    let v0: Double, tau: Double, vmin: Double
    var v: Double = 0
    mutating func step(_ dt: Double) -> Double {
        if abs(v) < vmin { v = v0 }
        let d = v * dt
        v *= exp(-dt / tau)
        return d
    }
}

final class Bench: NSObject {
    let cfg: Config
    let model: DataModel
    let grid: GridImpl
    let gutter: GutterView
    let window: NSWindow
    let columns: Columns
    var result: [String: Any] = [:]

    private var link: CADisplayLink!
    private var lastTs: CFTimeInterval = 0
    private var lastWall: CFTimeInterval = 0
    private var frameDurations: [Double] = []
    private var recording: String?      // phase that scrolled in the previous frame
    private var intervals: [String: [Double]] = [:]
    private var wallIntervals: [String: [Double]] = [:]
    private var phaseDurations: [String: Double] = [:]
    private var stages: [(String, (Double) -> Bool)] = []
    private var stageStart: CFTimeInterval = 0
    private var frameInStage = 0
    private var peakMB = 0.0
    private var tickCount = 0
    private var peakHeapMB = 0.0
    private var occlusionEvents: [String] = []
    // Main-thread busy time between display-link callbacks: the frame's wall
    // time minus the time the run loop slept. It covers layout, drawing and
    // the Core Animation commit. Unlike frame intervals it shows headroom.
    private var sleepT: CFTimeInterval = 0
    private var idleAcc: CFTimeInterval = 0
    private var busy: [String: [Double]] = [:]
    private var observers: [CFRunLoopObserver] = []
    private var dt = 1.0 / 120
    private let tDidFinish: Double
    private var editT0: CFTimeInterval = 0

    init(cfg: Config, model: DataModel, grid: GridImpl, gutter: GutterView, window: NSWindow,
         columns: Columns, tDidFinish: Double, autosizeMs: Double, buildMs: Double) {
        self.cfg = cfg
        self.model = model
        self.grid = grid
        self.gutter = gutter
        self.window = window
        self.columns = columns
        self.tDidFinish = tDidFinish
        super.init()
        result["impl"] = cfg.impl
        result["variant"] = cfg.impl == "table" ? (cfg.lite ? "A-lite" : cfg.flatten ? "A-flat" : "A") : "B"
        result["speed"] = cfg.speed
        result["flingRows"] = cfg.flingRows
        result["cols"] = cfg.cols
        result["rows"] = cfg.rows
        result["styling"] = cfg.styling
        result["editor"] = cfg.editor
        result["autosizeMs"] = autosizeMs
        result["autosizeSampleRows"] = cfg.sampleRows
        result["buildWindowMs"] = buildMs
        result["contentWidth"] = Double(columns.total)
        result["hwModel"] = sysctlString("hw.model")
        result["cpu"] = sysctlString("machdep.cpu.brand_string")
        result["osVersion"] = ProcessInfo.processInfo.operatingSystemVersionString
        result["screenMaxFPS"] = window.screen?.maximumFramesPerSecond ?? 0
        result["screenName"] = window.screen?.localizedName ?? "?"
        result["backingScale"] = Double(window.backingScaleFactor)
    }

    func start() {
        link = window.contentView!.displayLink(target: self, selector: #selector(tick(_:)))
        link.preferredFrameRateRange = CAFrameRateRange(minimum: 120, maximum: 120, preferred: 120)
        link.add(to: .main, forMode: .common)
        NotificationCenter.default.addObserver(forName: NSWindow.didChangeOcclusionStateNotification,
                                               object: window, queue: nil) { [unowned self] _ in
            let t = CACurrentMediaTime() - self.stageStart
            self.occlusionEvents.append("\(self.stages.first?.0 ?? "done")+\(String(format: "%.2f", t))s:"
                + (self.window.occlusionState.contains(.visible) ? "visible" : "occluded"))
        }
        // Busy = frame wall time minus the time the main run loop spent asleep.
        let sleep = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.beforeWaiting.rawValue, true, CFIndex.max) { [unowned self] _, _ in
            self.sleepT = CACurrentMediaTime()
        }!
        let wake = CFRunLoopObserverCreateWithHandler(nil, CFRunLoopActivity.afterWaiting.rawValue, true, CFIndex.min) { [unowned self] _, _ in
            if self.sleepT > 0 { self.idleAcc += CACurrentMediaTime() - self.sleepT }
            self.sleepT = 0
        }!
        for o in [wake, sleep] { CFRunLoopAddObserver(CFRunLoopGetMain(), o, .commonModes) }
        observers = [wake, sleep]
        buildStages()
        stageStart = CACurrentMediaTime()
    }

    // MARK: scrolling helpers

    private var clip: NSClipView { grid.scrollView.contentView }
    private var maxY: CGFloat { max(0, grid.documentView.frame.height - clip.bounds.height) }
    private var maxX: CGFloat { max(0, grid.documentView.frame.width - clip.bounds.width) }

    private func scroll(x: CGFloat? = nil, y: CGFloat? = nil) {
        let o = clip.bounds.origin
        let p = NSPoint(x: min(max(0, x ?? o.x), maxX), y: min(max(0, y ?? o.y), maxY))
        clip.scroll(to: p)
        grid.scrollView.reflectScrolledClipView(clip)
    }

    private func settle(_ seconds: Double, then: (() -> Void)? = nil) -> (Double) -> Bool {
        return { [unowned self] _ in
            if CACurrentMediaTime() - self.stageStart >= seconds { then?(); return true }
            return false
        }
    }

    private func verticalFlings(_ name: String, rows: Int) -> (Double) -> Bool {
        var fling = Fling(v0: cfg.speed, tau: 0.5, vmin: cfg.speed / 30)  // pt/s
        var travelled: CGFloat = 0
        let target = CGFloat(rows) * Metrics.rowHeight
        return { [unowned self] dt in
            let d = CGFloat(fling.step(dt))
            travelled += d
            self.scroll(y: self.clip.bounds.origin.y + d)
            self.recording = name
            return travelled >= target
        }
    }

    private func horizontal(_ name: String, right: Bool) -> (Double) -> Bool {
        let v0 = cfg.speed / 4
        var fling = Fling(v0: right ? v0 : -v0, tau: 0.4, vmin: v0 / 20)
        return { [unowned self] dt in
            let d = CGFloat(fling.step(dt))
            self.scroll(x: self.clip.bounds.origin.x + d)
            self.recording = name
            let x = self.clip.bounds.origin.x
            return right ? x >= self.maxX - 0.5 : x <= 0.5
        }
    }

    private func buildStages() {
        stages.append(("firstPaint", { [unowned self] _ in
            guard self.grid.drewOnce else { return false }
            let now = Date().timeIntervalSince1970
            self.result["launchToFirstFrameMs"] = (now - processStartTime()) * 1000
            self.result["didFinishToFirstFrameMs"] = (now - self.tDidFinish) * 1000
            return true
        }))
        stages.append(("openEditor", settle(0.3) { [unowned self] in
            if self.cfg.editor {
                self.grid.openEditor(row: 8, col: 2)
                self.gutter.needsDisplay = true
            }
        }))
        stages.append(("afterLoad", settle(1.0) { [unowned self] in
            self.result["footprintAfterLoadMB"] = footprintMB()
            self.result["layersAfterLoad"] = layerCount(self.window.contentView!.layer)
            self.result["heapAfterLoadMB"] = heapMB()
            self.result["visibleAtLoad"] = self.window.occlusionState.contains(.visible)
            self.result["onActiveSpace"] = self.window.isOnActiveSpace
            self.result["refreshMsMedian"] = median(self.frameDurations)
            if let path = self.cfg.snapshot {
                self.snapshot(path)
                self.finish()
            }
        }))
        if let path = cfg.snapshotEnd {
            // Check drawing precision at the far end of a 22M pt document:
            // jump to the last rows, put the active cell there, snapshot.
            stages.append(("snapEndJump", { [unowned self] _ in
                self.grid.openEditor(row: self.model.rows - 3, col: 2)
                self.scroll(y: self.maxY)
                self.gutter.needsDisplay = true
                return true
            }))
            stages.append(("snapEnd", settle(0.8) { [unowned self] in
                self.snapshot(path)
                self.finish()
            }))
            return
        }
        guard cfg.bench else { return }
        let half = cfg.flingRows / 2
        stages.append(("vertical", verticalFlings("vertical", rows: half)))
        stages.append(("horizontalRight", horizontal("horizontal", right: true)))
        stages.append(("verticalWide", verticalFlings("verticalWide", rows: half)))
        stages.append(("horizontalLeft", horizontal("horizontal", right: false)))
        stages.append(("jumpEnd", { [unowned self] _ in
            if self.frameInStage == 0 { self.scroll(y: self.maxY); self.recording = "jumpEnd" }
            return self.frameInStage >= 30
        }))
        stages.append(("jumpTop", { [unowned self] _ in
            if self.frameInStage == 0 { self.scroll(y: 0); self.recording = "jumpTop" }
            return self.frameInStage >= 30
        }))
        stages.append(("editOpen", { [unowned self] _ in
            self.grid.openEditor(row: 10, col: 2)
            self.gutter.needsDisplay = true
            return true
        }))
        stages.append(("editCommit", { [unowned self] _ in
            if self.frameInStage == 1 {
                self.editT0 = CACurrentMediaTime()
                let ok = self.grid.commitEditor(text: "Edited in spike")
                self.window.displayIfNeeded()
                self.result["editCommitAndDrawMs"] = (CACurrentMediaTime() - self.editT0) * 1000
                self.result["editVerified"] = ok && self.model.isEdited(10, 2)
            }
            if self.frameInStage == 2 {
                self.result["editToNextFrameMs"] = (CACurrentMediaTime() - self.editT0) * 1000
                return true
            }
            return false
        }))
        stages.append(("afterScroll", settle(1.0) { [unowned self] in
            self.result["footprintAfterScrollMB"] = footprintMB()
            self.result["layersAfterScroll"] = layerCount(self.window.contentView!.layer)
            self.result["heapAfterScrollMB"] = heapMB()
            self.result["footprintPeakMB"] = max(self.peakMB, footprintMB())
            self.result["heapPeakMB"] = max(self.peakHeapMB, heapMB())
            self.finish()
        }))
    }

    // MARK: per frame

    @objc private func tick(_ link: CADisplayLink) {
        let ts = link.timestamp
        let wall = CACurrentMediaTime()
        frameDurations.append((link.targetTimestamp - link.timestamp) * 1000)
        if lastTs > 0 {
            dt = min(0.05, ts - lastTs)
            if let phase = recording {
                busy[phase, default: []].append(max(0, (wall - lastWall) - idleAcc) * 1000)
                intervals[phase, default: []].append((ts - lastTs) * 1000)
                wallIntervals[phase, default: []].append((wall - lastWall) * 1000)
                phaseDurations[phase, default: 0] += ts - lastTs
            }
        }
        recording = nil
        idleAcc = 0
        lastTs = ts
        lastWall = wall
        tickCount += 1
        if tickCount % 15 == 0 {
            peakMB = max(peakMB, footprintMB())
            peakHeapMB = max(peakHeapMB, heapMB())
        }

        guard !stages.isEmpty else { return }
        let (_, step) = stages[0]
        if step(dt) {
            stages.removeFirst()
            stageStart = CACurrentMediaTime()
            frameInStage = 0
        } else {
            frameInStage += 1
        }
    }

    // MARK: output

    private func stats(_ xs: [Double], duration: Double, refresh: Double) -> [String: Any] {
        let s = xs.sorted()
        func pct(_ p: Double) -> Double { s.isEmpty ? 0 : s[min(s.count - 1, Int(Double(s.count - 1) * p + 0.5))] }
        let over1 = xs.filter { $0 > refresh * 1.5 }.count   // missed ≥1 refresh (>8.3 ms at 120 Hz)
        let over2 = xs.filter { $0 > refresh * 2.5 }.count   // missed ≥2 refreshes (>16.7 ms at 120 Hz)
        // Hitch time: how late the late frames were (Apple's "hitch time ratio").
        let hitchMs = xs.filter { $0 > refresh * 1.5 }.reduce(0) { $0 + ($1 - refresh) }
        return ["frames": xs.count, "p50": pct(0.5), "p95": pct(0.95), "p99": pct(0.99),
                "max": s.last ?? 0, "over8": over1, "over16": over2,
                "durationS": duration, "hitchMsPerS": duration > 0 ? hitchMs / duration : 0]
    }

    private func finish() {
        link.invalidate()
        let refresh = (result["refreshMsMedian"] as? Double) ?? (1000.0 / 120)
        var phases: [String: Any] = [:]
        var all: [Double] = []
        var allWall: [Double] = []
        var dur = 0.0
        for (k, v) in intervals {
            phases[k] = stats(v, duration: phaseDurations[k] ?? 0, refresh: refresh)
            if k != "jumpEnd" && k != "jumpTop" {
                all += v
                allWall += wallIntervals[k] ?? []
                dur += phaseDurations[k] ?? 0
            }
        }
        result["phases"] = phases
        result["scroll"] = stats(all, duration: dur, refresh: refresh)
        result["scrollWall"] = stats(allWall, duration: dur, refresh: refresh)
        var allBusy: [Double] = []
        var busyPhases: [String: Any] = [:]
        for (k, v) in busy {
            busyPhases[k] = stats(v, duration: 0, refresh: refresh)
            if k != "jumpEnd" && k != "jumpTop" { allBusy += v }
        }
        result["busyPhases"] = busyPhases
        result["busy"] = stats(allBusy, duration: 0, refresh: refresh)
        result["jumpEndMs"] = intervals["jumpEnd"]?.first ?? 0
        result["jumpTopMs"] = intervals["jumpTop"]?.first ?? 0
        result["counters"] = grid.counters
        result["windowVisible"] = window.occlusionState.contains(.visible)
        result["occlusionEvents"] = occlusionEvents
        result["pid"] = Int(getpid())

        let json = try! JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
        if let out = cfg.out {
            try? json.write(to: URL(fileURLWithPath: out))
        } else if cfg.bench {
            FileHandle.standardOutput.write(json)
        }
        if cfg.hold {
            FileHandle.standardError.write("holding; pid \(getpid())\n".data(using: .utf8)!)
            return
        }
        NSApp.terminate(nil)
    }

    private func snapshot(_ path: String) {
        // Screen recording permission isn't available, and cacheDisplay skips
        // layer-backed content, so render the window's own layer tree.
        guard let v = window.contentView, let layer = v.layer else { return }
        let scale = window.backingScaleFactor
        let w = Int(v.bounds.width * scale), h = Int(v.bounds.height * scale)
        guard let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0,
                                  space: CGColorSpace(name: CGColorSpace.sRGB)!,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return }
        ctx.scaleBy(x: scale, y: scale)
        layer.render(in: ctx)
        guard let img = ctx.makeImage() else { return }
        let rep = NSBitmapImageRep(cgImage: img)
        try? rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
    }
}

func layerCount(_ l: CALayer?) -> Int {
    guard let l else { return 0 }
    return 1 + (l.sublayers ?? []).reduce(0) { $0 + layerCount($1) }
}

func median(_ xs: [Double]) -> Double {
    let s = xs.sorted()
    return s.isEmpty ? 0 : s[s.count / 2]
}
