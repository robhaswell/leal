import AppKit
import UniformTypeIdentifiers

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
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = "Leal couldn’t open “\(url.lastPathComponent)”."
        alert.informativeText = Self.describe(error)
        alert.runModal()
    }

    /// A sentence for an error thrown by the Rust core.
    static func describe(_ error: any Error) -> String {
        switch error {
        case LealError.NotFound:
            "The file doesn’t exist."
        case let LealError.Io(_, message):
            message
        default:
            // Includes a Rust panic, which UniFFI throws as an internal error
            // whose description is the panic message.
            error.localizedDescription
        }
    }

    func windowWillClose(_ notification: Notification) {
        guard let closing = notification.object as? NSWindow else { return }
        windows.removeAll { $0 === closing }
    }
}
