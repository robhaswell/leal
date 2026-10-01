import AppKit
import LealFFI

/// The menu bar, built in code because the app has no storyboard.
@MainActor
enum MainMenu {
    /// Keeps the Open Recent menu's delegate alive (`NSMenu.delegate` is weak).
    private static let recent = RecentDocumentsMenu()

    static func make() -> NSMenu {
        let menuBar = NSMenu()
        menuBar.addItem(submenuItem(appMenu()))
        menuBar.addItem(submenuItem(fileMenu()))
        menuBar.addItem(submenuItem(editMenu()))
        menuBar.addItem(submenuItem(viewMenu()))
        let window = windowMenu()
        menuBar.addItem(submenuItem(window))
        NSApp.windowsMenu = window
        return menuBar
    }

    private static func appMenu() -> NSMenu {
        let menu = NSMenu(title: "Leal")
        menu.addItem(
            withTitle: String(localized: "About Leal", comment: "App menu"),
            action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)),
            keyEquivalent: ""
        )
        menu.addItem(.separator())
        menu.addItem(withTitle: String(localized: "Hide Leal", comment: "App menu"), action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        let hideOthers = menu.addItem(
            withTitle: String(localized: "Hide Others", comment: "App menu"),
            action: #selector(NSApplication.hideOtherApplications(_:)),
            keyEquivalent: "h"
        )
        hideOthers.keyEquivalentModifierMask = [.command, .option]
        menu.addItem(
            withTitle: String(localized: "Show All", comment: "App menu"),
            action: #selector(NSApplication.unhideAllApplications(_:)),
            keyEquivalent: ""
        )
        menu.addItem(.separator())
        menu.addItem(withTitle: String(localized: "Quit Leal", comment: "App menu"), action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        return menu
    }

    private static func fileMenu() -> NSMenu {
        let menu = NSMenu(title: String(localized: "File", comment: "Menu title"))
        // NSDocumentController is in the responder chain after the app
        // delegate, so these reach it with no target.
        menu.addItem(
            withTitle: String(localized: "Open…", comment: "File menu"),
            action: #selector(NSDocumentController.openDocument(_:)),
            keyEquivalent: "o"
        )
        let openRecent = NSMenuItem(title: String(localized: "Open Recent", comment: "File menu"), action: nil, keyEquivalent: "")
        let recentMenu = NSMenu(title: openRecent.title)
        recentMenu.delegate = recent
        openRecent.submenu = recentMenu
        menu.addItem(openRecent)
        menu.addItem(.separator())
        menu.addItem(withTitle: String(localized: "Close", comment: "File menu"), action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        menu.addItem(.separator())
        // Task 1.9: open the file again as it is on disk now, also after
        // Keep Editing hid the banner. Validated by the view controller.
        menu.addItem(
            withTitle: String(localized: "Reload from Disk", comment: "File menu: open the file again as it is on disk now (task 1.9)"),
            action: #selector(DocumentViewController.reloadFromDisk(_:)),
            keyEquivalent: ""
        )
        // ADR-0005 decision 8. Each item is validated by the document's
        // view controller: the encodings its BOM allows, the one in use
        // ticked.
        let reopen = NSMenuItem(title: StatusText.reopenWithEncoding, action: nil, keyEquivalent: "")
        let encodings = NSMenu(title: reopen.title)
        for encoding in Self.encodings {
            let item = encodings.addItem(
                withTitle: StatusText.encodingName(encoding),
                action: #selector(DocumentViewController.reopenWithEncoding(_:)),
                keyEquivalent: ""
            )
            item.representedObject = EncodingBox(encoding)
        }
        reopen.submenu = encodings
        menu.addItem(reopen)
        return menu
    }

    /// Every encoding Leal reads (ADR-0005 decision 5), in the core's order.
    static let encodings: [TextEncoding] = [
        .utf8, .utf16Le, .utf16Be, .windows1252, .windows1250, .windows1251, .windows1253, .windows1254,
        .windows1255, .windows1256, .windows1257, .windows1258, .iso88591, .iso88592, .iso885915, .macRoman,
    ]

    /// The delimiter of a View > Treat As item.
    static func delimiter(of item: NSMenuItem) -> Delimiter? {
        (item.representedObject as? DelimiterBox)?.delimiter
    }

    /// The encoding of a File > Reopen with Encoding item.
    static func encoding(of item: NSMenuItem) -> TextEncoding? {
        (item.representedObject as? EncodingBox)?.encoding
    }

    /// View > Show Cell Inspector, and Hide once it shows.
    static let showInspector = String(localized: "Show Cell Inspector", comment: "View menu: show the cell inspector pane (⌘I, mockup 05a)")
    static let hideInspector = String(localized: "Hide Cell Inspector", comment: "View menu: hide the cell inspector pane (⌘I)")

    /// The standard items, which text fields need for their shortcuts (0.3
    /// notes); the grid answers Copy and Select All itself (task 1.8). Then
    /// Find (DESIGN §4.2: ⌘F, ⌘G, ⇧⌘G) and Go to Row (⌘L).
    private static func editMenu() -> NSMenu {
        let menu = NSMenu(title: String(localized: "Edit", comment: "Menu title"))
        menu.addItem(withTitle: String(localized: "Undo", comment: "Edit menu"), action: Selector(("undo:")), keyEquivalent: "z")
        let redo = menu.addItem(withTitle: String(localized: "Redo", comment: "Edit menu"), action: Selector(("redo:")), keyEquivalent: "z")
        redo.keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(.separator())
        menu.addItem(withTitle: String(localized: "Cut", comment: "Edit menu"), action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        menu.addItem(withTitle: String(localized: "Copy", comment: "Edit menu"), action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        menu.addItem(withTitle: String(localized: "Paste", comment: "Edit menu"), action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        menu.addItem(withTitle: String(localized: "Delete", comment: "Edit menu"), action: #selector(NSText.delete(_:)), keyEquivalent: "")
        menu.addItem(withTitle: String(localized: "Select All", comment: "Edit menu"), action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        menu.addItem(.separator())
        let find = NSMenuItem(title: String(localized: "Find", comment: "Edit menu: the Find submenu"), action: nil, keyEquivalent: "")
        let findMenu = NSMenu(title: find.title)
        findMenu.addItem(
            withTitle: String(localized: "Find…", comment: "Edit > Find: show the find bar (⌘F, mockup 04a)"),
            action: #selector(DocumentViewController.showFind(_:)),
            keyEquivalent: "f"
        )
        findMenu.addItem(
            withTitle: String(localized: "Find Next", comment: "Edit > Find: the next match (⌘G)"),
            action: #selector(DocumentViewController.findNext(_:)),
            keyEquivalent: "g"
        )
        let previous = findMenu.addItem(
            withTitle: String(localized: "Find Previous", comment: "Edit > Find: the previous match (⇧⌘G)"),
            action: #selector(DocumentViewController.findPrevious(_:)),
            keyEquivalent: "g"
        )
        previous.keyEquivalentModifierMask = [.command, .shift]
        find.submenu = findMenu
        menu.addItem(find)
        menu.addItem(
            withTitle: String(localized: "Go to Row…", comment: "Edit menu: go to a row by its number (⌘L)"),
            action: #selector(DocumentViewController.goToRow(_:)),
            keyEquivalent: "l"
        )
        return menu
    }

    private static func viewMenu() -> NSMenu {
        let menu = NSMenu(title: String(localized: "View", comment: "Menu title"))
        menu.addItem(
            withTitle: String(localized: "Use First Row as Header", comment: "View menu: the Header row toggle (ADR-0002 question 13)"),
            action: #selector(DocumentViewController.toggleHeaderRow(_:)),
            keyEquivalent: ""
        )
        let treatAs = NSMenuItem(title: StatusText.treatAs, action: nil, keyEquivalent: "")
        let delimiters = NSMenu(title: treatAs.title)
        for delimiter in [Delimiter.comma, .semicolon, .tab, .pipe] {
            let item = delimiters.addItem(
                withTitle: StatusText.delimiter(delimiter),
                action: #selector(DocumentViewController.treatAsDelimiter(_:)),
                keyEquivalent: ""
            )
            item.representedObject = DelimiterBox(delimiter)
        }
        treatAs.submenu = delimiters
        menu.addItem(treatAs)
        menu.addItem(.separator())
        menu.addItem(
            withTitle: String(localized: "Show Irregularities", comment: "View menu: the diagnostics details popover (mockup 03b)"),
            action: #selector(DocumentViewController.showDetails(_:)),
            keyEquivalent: ""
        )
        menu.addItem(withTitle: showInspector, action: #selector(DocumentViewController.toggleCellInspector(_:)), keyEquivalent: "i")
        return menu
    }

    private static func windowMenu() -> NSMenu {
        let menu = NSMenu(title: String(localized: "Window", comment: "Menu title"))
        menu.addItem(withTitle: String(localized: "Minimize", comment: "Window menu"), action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        menu.addItem(withTitle: String(localized: "Zoom", comment: "Window menu"), action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        menu.addItem(
            withTitle: String(localized: "Bring All to Front", comment: "Window menu"),
            action: #selector(NSApplication.arrangeInFront(_:)),
            keyEquivalent: ""
        )
        return menu
    }

    private static func submenuItem(_ menu: NSMenu) -> NSMenuItem {
        let item = NSMenuItem(title: menu.title, action: nil, keyEquivalent: "")
        item.submenu = menu
        return item
    }
}

/// Fills File > Open Recent from `NSDocumentController`'s list each time it
/// opens. (A menu built in code doesn't get AppKit's automatic one.)
@MainActor
final class RecentDocumentsMenu: NSObject, NSMenuDelegate {
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let controller = NSDocumentController.shared
        for url in controller.recentDocumentURLs {
            let item = NSMenuItem(
                title: FileManager.default.displayName(atPath: url.path(percentEncoded: false)),
                action: #selector(openRecent(_:)),
                keyEquivalent: ""
            )
            item.target = self
            item.representedObject = url
            let icon = NSWorkspace.shared.icon(forFile: url.path(percentEncoded: false))
            icon.size = NSSize(width: 16, height: 16)
            item.image = icon
            menu.addItem(item)
        }
        if !controller.recentDocumentURLs.isEmpty {
            menu.addItem(.separator())
        }
        menu.addItem(
            withTitle: String(localized: "Clear Menu", comment: "File > Open Recent"),
            action: #selector(NSDocumentController.clearRecentDocuments(_:)),
            keyEquivalent: ""
        )
    }

    @objc private func openRecent(_ sender: NSMenuItem) {
        guard let url = sender.representedObject as? URL else { return }
        NSDocumentController.shared.openDocument(withContentsOf: url, display: true) { _, _, _ in }
    }
}
