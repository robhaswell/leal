import AppKit
import ObjectiveC
import XCTest

/// Tests never wait for a person. The ones that reach a question stub it
/// (`CSVDocument.showSheet`, `showAlert`, `chooseSaveAsDestination`,
/// `DocumentViewController.showAlert`, ...); this is the net under them: an
/// alert or a save panel that gets through to AppKit unstubbed would sit on
/// the screen until somebody clicked it, so it fails the running test
/// instead (naming the alert), and answers as cancelling: the alert's last
/// button, the panel's Cancel. Nothing is shown.
///
/// It lives in the test bundle alone (swizzled in when the bundle loads, as
/// its `NSPrincipalClass`, see `project.yml`), so the app has no code for
/// it.
@objc(LealAppTestsPrincipal)
final class NoRealUI: NSObject {
    override init() {
        super.init()
        Self.install()
    }

    nonisolated(unsafe) private static var installed = false

    /// The alerts and panels that got through, for the net's own test.
    nonisolated(unsafe) static var caught: [String] = []
    /// While set, a caught alert is recorded in `caught`, not failed.
    nonisolated(unsafe) static var expecting = false

    private static func report(_ what: String) {
        MainActor.assumeIsolated {
            caught.append(what)
            if !expecting {
                XCTFail("a real \(what) was shown: tests must stub it, never wait for a person")
            }
        }
    }

    /// Reports `alert` as caught and answers as its way out (its last button).
    private static func refuse(_ alert: NSAlert, _ how: String) -> Int {
        MainActor.assumeIsolated {
            report("alert \"\(alert.messageText)\" (\(how))")
            return NSApplication.ModalResponse.alertFirstButtonReturn.rawValue + max(alert.buttons.count - 1, 0)
        }
    }

    static func install() {
        guard !installed else { return }
        installed = true

        replace(NSAlert.self, #selector(NSAlert.runModal)) {
            let block: @convention(block) (NSAlert) -> Int = { alert in
                refuse(alert, "runModal")
            }
            return block
        }
        replace(NSAlert.self, #selector(NSAlert.beginSheetModal(for:completionHandler:))) {
            let block: @convention(block) (NSAlert, NSWindow, (@convention(block) (Int) -> Void)?) -> Void = { alert, _, done in
                done?(refuse(alert, "sheet"))
            }
            return block
        }
        replace(NSSavePanel.self, #selector(NSSavePanel.runModal)) {
            let block: @convention(block) (NSSavePanel) -> Int = { _ in
                report("save panel (runModal)")
                return NSApplication.ModalResponse.cancel.rawValue
            }
            return block
        }
        replace(NSSavePanel.self, #selector(NSSavePanel.beginSheetModal(for:completionHandler:))) {
            let block: @convention(block) (NSSavePanel, NSWindow, (@convention(block) (Int) -> Void)?) -> Void = { _, _, done in
                report("save panel (sheet)")
                done?(NSApplication.ModalResponse.cancel.rawValue)
            }
            return block
        }
        replace(NSSavePanel.self, #selector(NSSavePanel.begin(completionHandler:))) {
            let block: @convention(block) (NSSavePanel, (@convention(block) (Int) -> Void)?) -> Void = { _, done in
                report("save panel")
                done?(NSApplication.ModalResponse.cancel.rawValue)
            }
            return block
        }
    }

    private static func replace(_ type: AnyClass, _ selector: Selector, _ make: () -> Any) {
        guard let method = class_getInstanceMethod(type, selector) else {
            fatalError("NoRealUI: \(type) has no \(selector)")
        }
        method_setImplementation(method, imp_implementationWithBlock(make()))
    }
}

/// The net catches what it should, and quietly (`NoRealUI.expecting`).
@MainActor
final class NoRealUITests: XCTestCase {
    func testAnUnstubbedAlertOrPanelIsCaughtNotShown() {
        NoRealUI.install()
        NoRealUI.expecting = true
        defer { NoRealUI.expecting = false }
        NoRealUI.caught = []

        let alert = NSAlert()
        alert.messageText = "Question"
        alert.addButton(withTitle: "Yes")
        alert.addButton(withTitle: "No")
        XCTAssertEqual(alert.runModal(), .alertSecondButtonReturn, "the way out")
        var sheet: NSApplication.ModalResponse?
        alert.beginSheetModal(for: NSWindow()) { sheet = $0 }
        XCTAssertEqual(sheet, .alertSecondButtonReturn)

        let panel = NSSavePanel()
        XCTAssertEqual(panel.runModal(), .cancel)
        var chosen: NSApplication.ModalResponse?
        panel.beginSheetModal(for: NSWindow()) { chosen = $0 }
        XCTAssertEqual(chosen, .cancel)
        panel.begin { chosen = $0 }
        XCTAssertEqual(NoRealUI.caught.count, 5)
    }
}
