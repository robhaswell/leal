import AppKit

// Only in the builds `just snapshot` makes (`LEAL_BENCH`), like the rest
// of the scripted runs.
#if LEAL_BENCH

/// A sheet of the keyboard shortcuts phase 1 has, for the comparison with
/// mockup 06c (task 1.8): each menu command's title and key equivalent as
/// the running app's menu bar has them, and the grid's keys from AppKit's
/// standard key bindings (`GridMove`). It reads the shortcuts; it doesn't
/// restate them, so the picture shows what is wired up.
///
/// The words that aren't menu titles are for this picture only, which is
/// never shipped, so they aren't in the String Catalog.
@MainActor
enum ShortcutSheet {
    static func view(menu: NSMenu) -> NSView {
        func item(_ action: Selector) -> NSMenuItem? {
            func search(_ menu: NSMenu) -> NSMenuItem? {
                for item in menu.items {
                    if item.action == action { return item }
                    if let submenu = item.submenu, let found = search(submenu) { return found }
                }
                return nil
            }
            return search(menu)
        }
        func keys(_ action: Selector) -> String {
            guard let item = item(action) else { return "—" }
            var text = ""
            let modifiers = item.keyEquivalentModifierMask
            if modifiers.contains(.control) { text += "⌃" }
            if modifiers.contains(.option) { text += "⌥" }
            if modifiers.contains(.shift) { text += "⇧" }
            if modifiers.contains(.command) { text += "⌘" }
            return text + item.keyEquivalent.uppercased()
        }
        func title(_ action: Selector) -> String {
            item(action)?.title ?? "—"
        }
        let rows: [(String, String, String)] = [
            ("Move", "↑ ↓ ← →  ·  Page Up / Page Down", "⌘↑ / ⌘↓ first and last row; Tab, ⇧Tab"),
            ("Select", "⇧ with the move keys  ·  ⇧-click  ·  drag", "click a row number for the row"),
            (title(#selector(NSText.selectAll(_:))), keys(#selector(NSText.selectAll(_:))), "every row, to the end of the file"),
            ("Find", "\(keys(#selector(DocumentViewController.showFind(_:))))  ·  \(keys(#selector(DocumentViewController.findNext(_:)))) next", "\(keys(#selector(DocumentViewController.findPrevious(_:)))) previous"),
            (title(#selector(DocumentViewController.goToRow(_:))), keys(#selector(DocumentViewController.goToRow(_:))), "past the indexed rows too"),
            (title(#selector(NSText.copy(_:))), keys(#selector(NSText.copy(_:))), "TSV on the clipboard; paste is task 2.6"),
            (title(#selector(DocumentViewController.toggleCellInspector(_:))), keys(#selector(DocumentViewController.toggleCellInspector(_:))), "shows long or multiline values; editing is task 2.5"),
            ("Edit cell, Undo, Insert row", "phase 2", "tasks 2.5 and 2.5a"),
        ]
        let grid = NSGridView(numberOfColumns: 3, rows: 0)
        grid.rowSpacing = 18
        grid.columnSpacing = 28
        for (name, key, note) in rows {
            let label = NSTextField(labelWithString: name)
            label.font = .systemFont(ofSize: 14, weight: .semibold)
            let shortcut = NSTextField(labelWithString: key)
            shortcut.font = .systemFont(ofSize: 14)
            let detail = NSTextField(labelWithString: note)
            detail.font = .systemFont(ofSize: 13)
            detail.textColor = .secondaryLabelColor
            grid.addRow(with: [label, shortcut, detail])
        }
        let heading = NSTextField(labelWithString: "Keyboard shortcuts")
        heading.font = .systemFont(ofSize: 24, weight: .bold)
        let subtitle = NSTextField(labelWithString: "Read from Leal’s menu bar and key bindings (task 1.8).")
        subtitle.font = .systemFont(ofSize: 13)
        subtitle.textColor = .secondaryLabelColor
        let stack = NSStackView(views: [heading, subtitle, grid])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 10
        stack.setCustomSpacing(28, after: subtitle)
        stack.edgeInsets = NSEdgeInsets(top: 32, left: 32, bottom: 32, right: 32)
        stack.frame = NSRect(origin: .zero, size: stack.fittingSize)
        return stack
    }

    /// Draws the sheet to a PNG at 1×, on the window background.
    static func writePNG(menu: NSMenu, to url: URL) {
        let content = view(menu: menu)
        let window = NSWindow(contentRect: content.frame, styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = content
        content.layoutSubtreeIfNeeded()
        defer { window.close() }
        let bounds = content.bounds
        guard let rep = content.bitmapImageRepForCachingDisplay(in: bounds) else { return }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        NSColor.windowBackgroundColor.setFill()
        bounds.fill()
        NSGraphicsContext.restoreGraphicsState()
        content.cacheDisplay(in: bounds, to: rep)
        try? rep.representation(using: .png, properties: [:])?.write(to: url)
    }
}

#endif
