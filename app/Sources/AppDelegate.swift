import AppKit
import LealFFI
import UniformTypeIdentifiers
import os

/// Opens files and keeps their windows alive.
///
/// This is the walking skeleton (PLAN 0.3): File > Open shows what the Rust
/// core reads from a file. Task 1.6 replaces it with an `NSDocument`.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    /// Open summary windows. AppKit doesn't retain them for us.
    private var windows: [NSWindow] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = MainMenu.make()
        Self.removeLeftoverTemporaryFolders()
    }

    /// Removes the clones and copies that a crash left behind (DESIGN §3.1),
    /// off the main thread so it never delays launch.
    private static func removeLeftoverTemporaryFolders() {
        DispatchQueue.global(qos: .utility).async {
            do {
                let removed = try removeLeftoverTempFolders(temp: TemporaryFolders.locations())
                if removed > 0 {
                    Logger.open.info("Removed \(removed) leftover temporary folders")
                }
            } catch {
                Logger.open.error("Couldn’t remove leftover temporary folders: \(String(describing: error))")
            }
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Files opened from Finder, the Dock or `open -a Leal file.csv`.
    func application(_ application: NSApplication, open urls: [URL]) {
        urls.forEach(open)
    }

    /// File > Open…
    @objc func openDocument(_ sender: Any?) {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.commaSeparatedText, .tabSeparatedText]
        panel.allowsMultipleSelection = true
        guard panel.runModal() == .OK else { return }
        panel.urls.forEach(open)
    }

    private func open(_ url: URL) {
        do {
            let summary = try inspectFile(path: url.path(percentEncoded: false))
            let window = SummaryWindow.make(url: url, summary: summary, coreVersion: coreVersion())
            window.delegate = self
            windows.append(window)
            window.makeKeyAndOrderFront(nil)
        } catch {
            presentOpenError(error, url: url)
        }
    }

    private func presentOpenError(_ error: any Error, url: URL) {
        // The core's English description goes to the log only.
        Logger.open.error("Couldn’t open \(url.path(percentEncoded: false)): \(String(describing: error))")
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = OpenErrorText.title(fileName: url.lastPathComponent)
        alert.informativeText = OpenErrorText.describe(error)
        alert.runModal()
    }

    func windowWillClose(_ notification: Notification) {
        guard let closing = notification.object as? NSWindow else { return }
        windows.removeAll { $0 === closing }
    }
}

extension Logger {
    /// Opening files.
    static let open = Logger(subsystem: "io.github.robhaswell.leal", category: "open")
}
