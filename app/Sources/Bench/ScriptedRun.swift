import AppKit
import os

// Only in the builds `just bench-scroll` and `just snapshot` make
// (`LEAL_BENCH`): the shipped app has no scripted runs (CI checks with nm).
#if LEAL_BENCH

/// Runs the app by itself, from launch arguments, so it can be measured
/// and photographed without sending input to the system (CLAUDE.md). The
/// options are `-Name value` pairs, which macOS reads into the argument
/// defaults domain (a bare value would be opened as a document):
///
/// - `-LealBenchScroll <file.json>`: once the first document has drawn, a
///   display link scrolls the grid frame by frame (`ScrollBench`) and the
///   frame times and memory are written to the file, then the app quits.
///   `-LealBenchSpeed moderate` uses the spike's slower profile;
///   `-LealBenchVerticalOnly YES` stops after the first vertical flings.
/// - `-LealSnapshot <file.png>`: once indexing is done (or at once with
///   `-LealSnapshotEarly YES`), the window is drawn offscreen to a PNG at
///   1×, then the app quits. `-LealSelect row,column` picks the active
///   cell; `-LealJumpEnd YES` presses ⌘↓ first.
/// - `-LealAppearance light|dark` and `-LealWindowSize 1000x640` (the
///   content size, in points) for either.
///
/// Relative paths are in the app's temporary folder (in the sandbox
/// container). Open the file with `open -a Leal.app file.csv --args …`, so
/// the sandbox lets the app read it. `just bench-scroll` and `just
/// snapshot` do this.
@MainActor
final class ScriptedRun {
    private static var current: ScriptedRun?

    private let defaults: UserDefaults
    private var started = false

    private init(defaults: UserDefaults) {
        self.defaults = defaults
    }

    static func startIfAsked(defaults: UserDefaults) {
        let run = ScriptedRun(defaults: defaults)
        guard run.value(of: "LealBenchScroll") != nil || run.value(of: "LealSnapshot") != nil else { return }
        switch run.value(of: "LealAppearance") {
        case "dark": NSApp.appearance = NSAppearance(named: .darkAqua)
        case "light": NSApp.appearance = NSAppearance(named: .aqua)
        default: break
        }
        current = run
    }

    /// A document's window is on screen.
    static func documentShown(_ document: CSVDocument) {
        current?.begin(with: document)
    }

    func value(of option: String) -> String? {
        defaults.string(forKey: option)
    }

    func has(_ option: String) -> Bool {
        defaults.bool(forKey: option)
    }

    /// A path for an output file: relative ones go in the temporary folder.
    static func outputURL(_ path: String) -> URL {
        path.hasPrefix("/")
            ? URL(filePath: path)
            : FileManager.default.temporaryDirectory.appending(path: path)
    }

    private func begin(with document: CSVDocument) {
        guard !started, let controller = document.windowControllers.first as? DocumentWindowController else { return }
        started = true
        let content = controller.content
        if let size = value(of: "LealWindowSize")?.split(separator: "x").compactMap({ Double($0) }), size.count == 2 {
            controller.window?.setContentSize(NSSize(width: size[0], height: size[1]))
            content.view.layoutSubtreeIfNeeded()
        }
        if let cell = value(of: "LealSelect")?.split(separator: ",").compactMap({ Int($0) }), cell.count == 2 {
            content.grid.select(CellPosition(row: cell[0], column: cell[1]))
        }
        if let out = value(of: "LealBenchScroll") {
            let bench = ScrollBench(document: document, content: content, profile: value(of: "LealBenchSpeed") ?? "fast")
            bench.start { result in
                Self.write(result, to: Self.outputURL(out))
                NSApp.terminate(nil)
            }
            keep = bench
        } else if let out = value(of: "LealSnapshot") {
            Task { @MainActor in
                await self.snapshot(content: content, to: Self.outputURL(out))
                NSApp.terminate(nil)
            }
        }
    }

    private var keep: AnyObject?

    private func snapshot(content: DocumentViewController, to url: URL) async {
        if !has("LealSnapshotEarly") {
            let deadline = Date().addingTimeInterval(60)
            while !content.model.isIndexComplete, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        if has("LealJumpEnd") {
            content.grid.move(.lastRow)
        }
        // Let sizing, the review and drawing settle.
        try? await Task.sleep(for: .milliseconds(has("LealSnapshotEarly") ? 30 : 600))
        guard let window = content.view.window else { return }
        Snapshot.writePNG(of: window, to: url)
        Logger.open.info("Snapshot written to \(url.path(percentEncoded: false), privacy: .public)")
    }

    private static func write(_ result: [String: Any], to url: URL) {
        do {
            let data = try JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
            try data.write(to: url)
            Logger.open.info("Benchmark written to \(url.path(percentEncoded: false), privacy: .public)")
        } catch {
            Logger.open.error("Couldn’t write the benchmark: \(String(describing: error), privacy: .public)")
        }
    }
}

/// Draws a window offscreen, title bar included, with
/// `cacheDisplay(in:to:)`: no Screen Recording permission is needed (0.3
/// notes). Saved at 1× (points), which keeps the PNG small.
@MainActor
enum Snapshot {
    static func image(of window: NSWindow) -> NSBitmapImageRep? {
        guard let frameView = window.contentView?.superview else { return nil }
        frameView.layoutSubtreeIfNeeded()
        let bounds = frameView.bounds
        guard let rep = frameView.bitmapImageRepForCachingDisplay(in: bounds) else { return nil }
        frameView.cacheDisplay(in: bounds, to: rep)
        return rep
    }

    static func writePNG(of window: NSWindow, to url: URL, scale: CGFloat = 1) {
        guard let rep = image(of: window) else { return }
        let size = NSSize(width: (CGFloat(rep.pixelsWide) / window.backingScaleFactor * scale).rounded(),
                          height: (CGFloat(rep.pixelsHigh) / window.backingScaleFactor * scale).rounded())
        guard let small = NSBitmapImageRep(
            bitmapDataPlanes: nil,
            pixelsWide: Int(size.width),
            pixelsHigh: Int(size.height),
            bitsPerSample: 8,
            samplesPerPixel: 4,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: .deviceRGB,
            bytesPerRow: 0,
            bitsPerPixel: 0
        ) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: small)
        NSGraphicsContext.current?.imageInterpolation = .high
        rep.draw(in: NSRect(origin: .zero, size: size))
        NSGraphicsContext.restoreGraphicsState()
        try? small.representation(using: .png, properties: [:])?.write(to: url)
    }
}

#endif
