import AppKit
import LealFFI
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
///   cell; `-LealJumpEnd YES` presses ⌘↓ first; `-LealDetails YES` opens
///   the diagnostics popover on its first kind's first occurrence (mockup
///   03b); `-LealFind text` searches for `text` from the selected cell in
///   the find bar (04a); `-LealInspector YES` shows the cell inspector
///   (05a); `-LealShortcuts YES` draws the shortcut sheet (06c) instead
///   of the window; `-LealWaitForChange YES` waits (up to 45 s) until the
///   file has changed on disk, so the task 1.9 banner shows, or its
///   simulated drive was disconnected. The app changes nothing: whoever
///   runs it changes the file.
///   For the phase 1 gate's screenshots of the UI with no mockup (cons-11):
///   `-LealWaitForReview YES` waits for the review, so a suggestion banner
///   shows; `-LealMenu treatAs|reopen` draws the status bar's Treat As or
///   Reopen with Encoding menu above its button; `-LealGoToRow YES` draws
///   the Go to Row sheet; `-LealFindWrap YES`, with `-LealFind`, steps
///   back past the first match, so the "wrapped" sign shows; and, in a
///   Debug build, whose core has the test hooks, `-LealSimulateFault
///   disconnect:<byte>` or `change:<byte>` opens the file as if on a
///   removable drive that vanishes, or whose file changes, at that byte
///   of the copy. Menus and sheets are windows of their own, which don't
///   draw offscreen: their content is drawn on a plain panel where they
///   would be, as for the details popover. `-LealMainMenu File` (or
///   paths such as `View,View/Treat_As`) draws menus of the menu bar, their
///   items validated by the window, under the title bar;
///   `-LealSimulateReadError YES` ends the index with a read error after
///   first paint (`JobFailure.Failed`, the "Partly read" banner); and
///   `-LealSimulateFault none` opens the file as if on a removable drive
///   that doesn't fail.
/// - `-LealReopen <n>`: once the document is indexed, close it and open
///   its file again, `n` times, then quit: each open's "Open to first rows"
///   signpost after the first is an open in a running app, without the
///   costs of the process's first window (`just perf`, task 1.10).
/// - `-LealAppearance light|dark` and `-LealWindowSize 1000x640` (the
///   content size, in points) for any of them.
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

    /// Before any document opens (`applicationWillFinishLaunching`): a
    /// simulated removable drive, if asked for.
    static func prepare(defaults: UserDefaults) {
        #if DEBUG
        guard let fault = defaults.string(forKey: "LealSimulateFault") else { return }
        let parts = fault.split(separator: ":")
        guard fault == "none" || parts.count == 2, let at = parts.count == 2 ? UInt64(parts[1]) : 0 else { return }
        let simulated: SimulatedFault?? = switch parts[0] {
        case "disconnect": .some(.disconnect(at: at))
        case "change": .some(.change(at: at))
        case "none": .some(nil)
        default: nil
        }
        guard let simulated else { return }
        DocumentModel.openForTesting = { path, environment, options, observer in
            try debugOpenDocumentWithFault(
                path: path,
                temp: environment.temp,
                scheduler: environment.scheduler,
                options: options,
                observer: observer,
                chunkBytes: 4096,
                fault: simulated
            )
        }
        #endif
    }

    static func startIfAsked(defaults: UserDefaults) {
        let run = ScriptedRun(defaults: defaults)
        guard ["LealBenchScroll", "LealSnapshot", "LealReopen"].contains(where: { run.value(of: $0) != nil }) else { return }
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
        } else if let count = value(of: "LealReopen").flatMap({ Int($0) }) {
            Task { @MainActor in
                await self.reopen(document, times: count)
                NSApp.terminate(nil)
            }
        } else if let out = value(of: "LealSnapshot") {
            Task { @MainActor in
                await self.snapshot(content: content, to: Self.outputURL(out))
                NSApp.terminate(nil)
            }
        }
    }

    private var keep: AnyObject?

    /// Closes `document` and opens its file again through the document
    /// controller, as Open Recent would, `times` times, each once the last
    /// has drawn its rows and finished indexing.
    private func reopen(_ document: CSVDocument, times: Int) async {
        var document = document
        for _ in 0..<times {
            guard await Self.settled(document), let url = document.fileURL else { return }
            document.close()
            let opened: NSDocument? = await withCheckedContinuation { continuation in
                NSDocumentController.shared.openDocument(withContentsOf: url, display: true) { document, _, _ in
                    continuation.resume(returning: document)
                }
            }
            guard let next = opened as? CSVDocument else { return }
            document = next
        }
        _ = await Self.settled(document)
    }

    /// Waits (up to 30 s) until the document's grid has drawn rows and its
    /// index is complete, then half a second more.
    private static func settled(_ document: CSVDocument) async -> Bool {
        let deadline = Date().addingTimeInterval(30)
        while Date() < deadline {
            if let content = (document.windowControllers.first as? DocumentWindowController)?.content,
               content.grid.gridView.firstDrawTime != nil, content.model.isIndexComplete
            {
                try? await Task.sleep(for: .milliseconds(500))
                return true
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        return false
    }

    private func snapshot(content: DocumentViewController, to url: URL) async {
        if !has("LealSnapshotEarly") {
            let deadline = Date().addingTimeInterval(60)
            while !content.model.isIndexComplete, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        if has("LealWaitForChange") {
            let deadline = Date().addingTimeInterval(45)
            while content.model.original.state == .unchanged, !content.model.changedOnDisk,
                  content.model.storage != .disconnected, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        if has("LealSimulateReadError") {
            // As if the drive had returned EIO part-way (app-8).
            let model = content.model
            model.jobEnded(JobFailure.Failed(message: "Input/output error"), job: .index, reading: model.readingID)
        }
        if has("LealWaitForReview") {
            let deadline = Date().addingTimeInterval(30)
            while content.model.review == nil, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        if has("LealJumpEnd") {
            content.grid.move(.lastRow)
        }
        if let query = value(of: "LealFind") {
            // The find bar (mockup 04a), searching from the selected cell.
            content.showFind(nil)
            content.findBar.field.stringValue = query
            content.search(for: query)
            let deadline = Date().addingTimeInterval(30)
            while content.find.isSearching || content.find.pendingStep != nil, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(20))
            }
        }
        if has("LealInspector") {
            // The cell inspector (mockup 05a), on the selected cell.
            content.setInspectorShown(true)
            await content.inspectorTask?.value
        }
        // Let sizing, the review and drawing settle.
        try? await Task.sleep(for: .milliseconds(has("LealSnapshotEarly") ? 30 : 600))
        if has("LealDetails") {
            // The details popover (mockup 03b), on its first kind's first
            // occurrence.
            content.showDetails(nil)
            if let kind = content.details?.entries.first?.kind {
                await content.navigate(kind, forward: true).value
            }
            try? await Task.sleep(for: .milliseconds(300))
        }
        if has("LealFindWrap") {
            // Back past the first match: round to the last, with the sign,
            // which shows for 0.7 s.
            let steps = content.find.steps
            content.findPrevious(nil)
            let deadline = Date().addingTimeInterval(10)
            while content.find.steps == steps, Date() < deadline {
                try? await Task.sleep(for: .milliseconds(5))
            }
            try? await Task.sleep(for: .milliseconds(30))
        }
        if has("LealShortcuts"), let menu = NSApp.mainMenu {
            // The shortcut sheet (mockup 06c) in place of the window.
            ShortcutSheet.writePNG(menu: menu, to: url)
            return
        }
        guard let window = content.view.window else { return }
        // The popover is a window of its own, placed by the system, whose
        // glass doesn't draw offscreen: draw its content on a plain panel
        // under the Details button, where the mockup has it.
        var panels: [(NSView, NSRect)] = []
        if let details = content.details?.view, content.detailsPopover != nil,
           case let anchor = content.detailsAnchor {
            let button = anchor.convert(anchor.bounds, to: nil)
            let size = details.fittingSize
            let x = min(button.maxX + 12, window.frame.width - 8) - size.width
            panels.append((details, NSRect(x: x, y: button.minY - 8 - size.height, width: size.width, height: size.height)))
        }
        if let which = value(of: "LealMenu") {
            // The status bar's menu, opening upwards from its button, as it
            // does at the bottom of the screen.
            let bar = content.statusBar
            let menu = which == "reopen" ? bar.reopenMenu() : bar.treatAsMenu()
            if let button = which == "reopen" ? bar.encodingButton : bar.delimiterButton {
                let frame = button.convert(button.bounds, to: nil)
                let picture = MenuPicture(menu: menu)
                let size = picture.frame.size
                panels.append((picture, NSRect(x: frame.minX - 8, y: frame.maxY + 4, width: size.width, height: size.height)))
            }
        }
        if let paths = value(of: "LealMainMenu") {
            // Each menu of the comma-separated paths, validated as the menu
            // bar would (by the window's controller, where it handles the
            // item); a submenu opens beside its item in the menu before it.
            var frames: [String: NSRect] = [:]
            for path in paths.split(separator: ",").map(String.init) {
                guard let menu = Self.menu(at: path) else { continue }
                for item in menu.items {
                    if let action = item.action, content.responds(to: action) {
                        item.isEnabled = content.validateMenuItem(item)
                    }
                }
                let picture = MenuPicture(menu: menu)
                let size = picture.frame.size
                var frame = NSRect(x: 10, y: window.contentLayoutRect.maxY - size.height - 2, width: size.width, height: size.height)
                let parentPath = path.split(separator: "/").dropLast().joined(separator: "/")
                if let parent = frames[parentPath], let parentMenu = Self.menu(at: parentPath),
                   let row = MenuPicture(menu: parentMenu).items.firstIndex(where: { $0.submenu === menu }) {
                    let rowTop = parent.maxY - MenuPicture.inset - CGFloat(row) * MenuPicture.rowHeight
                    frame.origin = NSPoint(x: parent.maxX - 4, y: rowTop + MenuPicture.inset - size.height)
                }
                frames[path] = frame
                panels.append((picture, frame))
            }
        }
        if has("LealGoToRow") {
            // The sheet hangs from the title bar, in the middle.
            let (alert, _) = content.goToRowAlert()
            alert.layout()
            if let sheet = alert.window.contentView {
                let size = sheet.frame.size
                let top = window.contentLayoutRect.maxY
                panels.append((sheet, NSRect(x: ((window.frame.width - size.width) / 2).rounded(), y: top - size.height, width: size.width, height: size.height)))
            }
        }
        Snapshot.writePNG(of: window, panels: panels, to: url)
        Logger.open.info("Snapshot written to \(url.path(percentEncoded: false), privacy: .public)")
    }

    /// The menu bar's menu at `path`, such as `File` or `View/Treat_As`
    /// ("_" for a space: `just snapshot` splits its options at spaces).
    private static func menu(at path: String) -> NSMenu? {
        var menu = NSApp.mainMenu
        for title in path.split(separator: "/").map({ $0.replacingOccurrences(of: "_", with: " ") }) {
            menu = menu?.items.first { $0.submenu?.title == title || $0.title == title }?.submenu
        }
        return menu
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

/// A menu's items as AppKit draws them in a menu, for a snapshot: menus are
/// windows of their own, which don't draw offscreen. The title, a check
/// mark by the item that is on, disabled items greyed.
@MainActor
final class MenuPicture: NSView {
    /// The items a menu shows: not hidden ones, nor the alternates that
    /// show only while Option is held.
    let items: [NSMenuItem]
    static let rowHeight: CGFloat = 22
    static let inset: CGFloat = 5
    private static let checkWidth: CGFloat = 22
    private static var font: NSFont { .menuFont(ofSize: 0) }

    init(menu: NSMenu) {
        items = menu.items.filter { !$0.isHidden && !$0.isAlternate }
        let attributes: [NSAttributedString.Key: Any] = [.font: Self.font]
        let widest = items.map { ($0.title as NSString).size(withAttributes: attributes).width }.max() ?? 0
        let keys = items.map { (Self.keyText($0) as NSString).size(withAttributes: attributes).width }.max() ?? 0
        let size = NSSize(
            width: (Self.checkWidth + widest + (keys > 0 ? keys + 28 : 0) + 30).rounded(.up),
            height: CGFloat(items.count) * Self.rowHeight + 2 * Self.inset
        )
        super.init(frame: NSRect(origin: .zero, size: size))
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not used")
    }

    override var isFlipped: Bool { true }

    /// An item's key equivalent as menus show it, such as "⌘O"; a submenu's
    /// arrow.
    private static func keyText(_ item: NSMenuItem) -> String {
        if item.hasSubmenu { return "›" }
        guard !item.keyEquivalent.isEmpty else { return "" }
        let mask = item.keyEquivalentModifierMask
        var text = ""
        if mask.contains(.control) { text += "⌃" }
        if mask.contains(.option) { text += "⌥" }
        if mask.contains(.shift) || item.keyEquivalent != item.keyEquivalent.lowercased() { text += "⇧" }
        if mask.contains(.command) { text += "⌘" }
        return text + item.keyEquivalent.uppercased()
    }

    override func draw(_ dirtyRect: NSRect) {
        for (index, item) in items.enumerated() {
            let top = Self.inset + CGFloat(index) * Self.rowHeight
            if item.isSeparatorItem {
                NSColor.separatorColor.setFill()
                NSRect(x: 10, y: top + Self.rowHeight / 2, width: bounds.width - 20, height: 1).fill()
                continue
            }
            let color: NSColor = item.isEnabled ? .labelColor : .tertiaryLabelColor
            let attributes: [NSAttributedString.Key: Any] = [.font: Self.font, .foregroundColor: color]
            let height = (item.title as NSString).size(withAttributes: attributes).height
            let y = top + (Self.rowHeight - height) / 2
            if item.state == .on {
                ("✓" as NSString).draw(at: NSPoint(x: 8, y: y), withAttributes: attributes)
            }
            (item.title as NSString).draw(at: NSPoint(x: Self.checkWidth, y: y), withAttributes: attributes)
            let key = Self.keyText(item) as NSString
            let width = key.size(withAttributes: attributes).width
            key.draw(at: NSPoint(x: bounds.width - 14 - width, y: y), withAttributes: attributes)
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

    /// `panels` are views drawn over the window on a plain rounded panel,
    /// each at a rectangle in the window's coordinates: the details
    /// popover's content.
    static func writePNG(of window: NSWindow, panels: [(NSView, NSRect)] = [], to url: URL, scale: CGFloat = 1) {
        guard let rep = image(of: window) else { return }
        let size = NSSize(width: (CGFloat(rep.pixelsWide) / window.backingScaleFactor * scale).rounded(),
                          height: (CGFloat(rep.pixelsHigh) / window.backingScaleFactor * scale).rounded())
        let panels = panels.compactMap { view, frame -> (NSBitmapImageRep, NSRect)? in
            view.layoutSubtreeIfNeeded()
            guard let image = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return nil }
            view.cacheDisplay(in: view.bounds, to: image)
            return (image, NSRect(x: frame.minX * scale, y: frame.minY * scale, width: frame.width * scale, height: frame.height * scale))
        }
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
        window.effectiveAppearance.performAsCurrentDrawingAppearance {
            for (image, frame) in panels {
                let panel = NSBezierPath(roundedRect: frame, xRadius: 10, yRadius: 10)
                NSGraphicsContext.saveGraphicsState()
                let shadow = NSShadow()
                shadow.shadowBlurRadius = 12
                shadow.shadowOffset = NSSize(width: 0, height: -3)
                shadow.shadowColor = NSColor.black.withAlphaComponent(0.25)
                shadow.set()
                NSColor.windowBackgroundColor.setFill()
                panel.fill()
                NSGraphicsContext.restoreGraphicsState()
                NSColor.separatorColor.setStroke()
                panel.stroke()
                image.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: false, hints: nil)
            }
        }
        NSGraphicsContext.restoreGraphicsState()
        try? small.representation(using: .png, properties: [:])?.write(to: url)
    }
}

#endif
