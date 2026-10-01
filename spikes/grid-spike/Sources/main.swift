import AppKit

// Grid spike entry point.
//   GridSpike --impl table|custom --cols N [--bench --out results.json]
//             [--snapshot out.png] [--no-styling] [--no-editor] [--hold]

protocol GridImpl: AnyObject {
    var scrollView: NSScrollView { get }
    var documentView: NSView { get }
    var drewOnce: Bool { get }
    var counters: [String: Int] { get }
    func install(in root: NSView, gutter: GutterView, gutterWidth: CGFloat)
    func syncOffsets(gutter: GutterView)
    func openEditor(row: Int, col: Int)
    /// Types `text` into the open editor and presses Return. True if the edit landed.
    func commitEditor(text: String) -> Bool
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    let cfg: Config
    var window: NSWindow!
    var grid: GridImpl!
    var gutter: GutterView!
    var bench: Bench!
    var activity: NSObjectProtocol?

    init(cfg: Config) { self.cfg = cfg }

    func applicationDidFinishLaunching(_ n: Notification) {
        let tDidFinish = Date().timeIntervalSince1970
        // Match the light-mode mockups (01a); appearance doesn't change the cost.
        if !cfg.dark { NSApp.appearance = NSAppearance(named: .aqua) }
        activity = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiated, .latencyCritical], reason: "grid spike measurement")

        let model = DataModel(rows: cfg.rows, cols: cfg.cols, styling: cfg.styling)
        let t0 = CACurrentMediaTime()
        let columns = Columns.autosize(model, sampleRows: cfg.sampleRows)
        let autosizeMs = (CACurrentMediaTime() - t0) * 1000

        // Use the fastest display (the built-in ProMotion panel on this Mac).
        let screens = NSScreen.screens
        for (i, s) in screens.enumerated() {
            FileHandle.standardError.write("screen \(i): \(s.localizedName) \(s.maximumFramesPerSecond) Hz \(s.frame)\n".data(using: .utf8)!)
        }
        let screen = cfg.screen.map { screens[$0] }
            ?? screens.max { $0.maximumFramesPerSecond < $1.maximumFramesPerSecond } ?? NSScreen.main!
        let size = Metrics.windowSize
        let vf = screen.visibleFrame
        let frame = NSRect(x: vf.midX - size.width / 2, y: vf.midY - size.height / 2,
                           width: size.width, height: size.height)
        window = NSWindow(contentRect: frame, styleMask: [.titled, .closable, .miniaturizable],
                          backing: .buffered, defer: false, screen: screen)
        window.title = "orders-2025.csv — \(cfg.impl) · \(cfg.cols) cols"
        window.isReleasedWhenClosed = false
        window.level = .floating  // stay unoccluded while measuring

        let t1 = CACurrentMediaTime()
        let root = NSView(frame: NSRect(origin: .zero, size: size))
        gutter = GutterView(model: model)
        let gw = GutterView.width(rows: model.rows)
        switch cfg.impl {
        case "table": grid = TableGrid(model: model, columns: columns, mode: .cells, clampPrepare: cfg.clampPrepare)
        case "table-lite": grid = TableGrid(model: model, columns: columns, mode: .lite, clampPrepare: cfg.clampPrepare)
        case "table-rowdraw": grid = TableGrid(model: model, columns: columns, mode: .rowdraw, clampPrepare: cfg.clampPrepare)
        default: grid = CustomGrid(model: model, columns: columns)
        }
        grid.install(in: root, gutter: gutter, gutterWidth: gw)
        let status = makeStatusBar(model)
        status.frame = NSRect(x: 0, y: 0, width: size.width, height: Metrics.statusHeight)
        root.addSubview(status)
        window.contentView = root

        let clip = grid.scrollView.contentView
        clip.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: clip, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.grid.syncOffsets(gutter: self.gutter)
        }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        FileHandle.standardError.write("window \(window.windowNumber)\n".data(using: .utf8)!)
        let buildMs = (CACurrentMediaTime() - t1) * 1000

        bench = Bench(cfg: cfg, model: model, grid: grid, gutter: gutter, window: window,
                      columns: columns, tDidFinish: tDidFinish, autosizeMs: autosizeMs, buildMs: buildMs)
        bench.start()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

let cfg = Config.parse()
let app = NSApplication.shared
let delegate = AppDelegate(cfg: cfg)
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
