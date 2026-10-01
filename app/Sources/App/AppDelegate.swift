import AppKit
import LealFFI
import os

/// The app's delegate. Opening files is `NSDocumentController`'s job
/// (File > Open, Open Recent, Finder, the Dock, `open -a`): this only sets
/// up the menus, removes leftover temporary folders and, in a build with
/// `LEAL_BENCH`, starts a scripted run if the launch arguments ask for one
/// (`ScriptedRun`).
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.mainMenu = MainMenu.make()
        // A test host opens nothing and doesn't take focus (0.3 notes).
        guard !Self.isTestHost else { return }
        Self.removeLeftoverTemporaryFolders()
        #if LEAL_BENCH
        // Only the `just bench-scroll` and `just snapshot` builds have it.
        ScriptedRun.startIfAsked(defaults: .standard)
        #endif
        // The launch budget's end (DESIGN §1): with no document Leal opens
        // no window, so this is when it is ready for File > Open.
        Signposts.launched()
    }

    /// Whether the app was launched to host the app's XCTests.
    static var isTestHost: Bool {
        ProcessInfo.processInfo.environment["XCTestConfigurationFilePath"] != nil
            || ProcessInfo.processInfo.environment["XCTestBundlePath"] != nil
            || NSClassFromString("XCTestCase") != nil
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
                Logger.open.error("Couldn’t remove leftover temporary folders: \(String(describing: error), privacy: .public)")
            }
        }
    }

    /// Leal is a viewer: launching it doesn't make an empty document.
    func applicationShouldOpenUntitledFile(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationSupportsSecureRestorableState(_ app: NSApplication) -> Bool {
        true
    }
}

extension Logger {
    /// Opening files.
    static let open = Logger(subsystem: "io.github.robhaswell.leal", category: "open")
}
