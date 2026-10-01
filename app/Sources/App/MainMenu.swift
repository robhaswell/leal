import AppKit

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
        return menu
    }

    /// The standard items, which text fields need for their shortcuts (0.3
    /// notes). The grid's own Copy and Select All are task 1.8.
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
        return menu
    }

    private static func viewMenu() -> NSMenu {
        let menu = NSMenu(title: String(localized: "View", comment: "Menu title"))
        menu.addItem(
            withTitle: String(localized: "Use First Row as Header", comment: "View menu: the Header row toggle (ADR-0002 question 13)"),
            action: #selector(DocumentViewController.toggleHeaderRow(_:)),
            keyEquivalent: ""
        )
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
