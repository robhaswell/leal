import AppKit
import QuartzCore
import LealFFI
import os

/// A CSV or TSV file open in Leal: an `NSDocument`, so Open Recent, window
/// tabs, restoration and (from phase 2) saving work the macOS way
/// (DESIGN §4.3). Info.plist names it as the `NSDocumentClass`, and every
/// open goes through `NSDocumentController`.
///
/// The file is opened by the core (`DocumentModel.open`): its first screen
/// is read before `read(from:ofType:)` returns, without waiting for the
/// index (DESIGN §3.10).
final class CSVDocument: NSDocument {
    /// The scheduler and temporary folders new documents use. Tests set
    /// their own.
    static var environment: () throws -> DocumentEnvironment = { try AppEnvironment.shared() }

    private(set) var model: DocumentModel?
    private var environment: DocumentEnvironment?
    /// The document has failed and offers to reopen the file (DESIGN §3.9):
    /// the alert is showing, or will be once the window is on screen.
    private(set) var isOfferingReopen = false
    /// The failure alert has been shown.
    private var failureShown = false
    /// When reading began (`CACurrentMediaTime`), for the open to first
    /// rows budget (DESIGN §1).
    private(set) var openStarted: CFTimeInterval?
    /// The "Open to first rows" signpost, which the grid's first draw with
    /// rows ends.
    private var opening: OSSignpostIntervalState?
    /// The file's modification date when it was opened, read off the main
    /// thread (`PendingOpens.Facts`), for `DocumentController` to set as
    /// `fileModificationDate` (task 2.0). `nil` when this document opened
    /// the file itself (tests).
    private(set) var openedModificationDate: Date?

    /// Leal writes the file only when the user saves (DESIGN §4.3).
    nonisolated override class var autosavesInPlace: Bool { false }

    /// Reading happens on the main thread, AppKit's default, so the model
    /// can be set up there. The file itself is read before that, off the
    /// main thread, by `DocumentController` (task 2.0): a file on a network
    /// share must never be read on the main thread (ADR-0009). AppKit's
    /// concurrent reading would make the `NSDocument` on a background queue,
    /// which Swift 6 doesn't allow for this main-actor class.
    nonisolated override class func canConcurrentlyReadDocuments(ofType typeName: String) -> Bool {
        false
    }

    nonisolated override func read(from url: URL, ofType typeName: String) throws {
        // `canConcurrentlyReadDocuments` is false, so AppKit reads on the
        // main thread (and tests call this there).
        try MainActor.assumeIsolated {
            if model == nil {
                try open(url)
            } else {
                // A second read, as `revert(toContentsOf:ofType:)` does: the
                // window keeps its model, which reads the file again, so it
                // is never left bound to a closed one (phase 1 review,
                // app-3). SEAM(2.5): Revert to Saved goes through `reload`
                // too, asking first if there are edits (ADR-0008 decision
                // 4, proposed).
                do {
                    try reload(from: url)
                } catch {
                    throw Self.openError(error, url: url)
                }
            }
        }
    }

    /// Makes the model for `url`. `DocumentController` has usually opened
    /// the core's document already, off the main thread (`PendingOpens`);
    /// otherwise (a test's `CSVDocument(contentsOf:ofType:)`) the core opens
    /// the file here, which is only safe for a file that isn't on a network
    /// share (ADR-0009: the core's debug builds check).
    private func open(_ url: URL) throws {
        let core: Result<DocumentModel.OpenedCore, any Error>
        if let pending = PendingOpens.take(url) {
            openStarted = pending.started
            opening = pending.opening
            environment = pending.environment
            openedModificationDate = pending.facts.modified
            core = pending.core
        } else {
            if !AppDelegate.isTestHost, !PendingOpens.wasSkipped(url) {
                // Every open in the app comes through `DocumentController`;
                // one that didn't reads the file on the main thread, which a
                // network share must never be (ADR-0009).
                Logger.document.fault("Opening \(url.lastPathComponent, privacy: .private) on the main thread: it didn't come through DocumentController")
            }
            openStarted = CACurrentMediaTime()
            opening = Signposts.opening()
            do {
                let environment = try Self.environment()
                self.environment = environment
                core = Result { try DocumentModel.openCore(url: url, environment: environment) }
            } catch {
                try fail(opening: error, url: url)
            }
        }
        try finishOpening(core, url: url)
    }

    /// The end of an open: the model from what the core opened, or the open
    /// error.
    private func finishOpening(_ core: Result<DocumentModel.OpenedCore, any Error>, url: URL) throws {
        do {
            let opened = try DocumentModel.make(from: core.get())
            // A panic while opening (in a call `init` makes) fails the
            // document before there is a window to show the failure in:
            // report it as an open error instead (DESIGN §3.9).
            if let failure = opened.failure {
                opened.close()
                throw failure
            }
            // The core follows the file when it is moved (task 1.9), and
            // the document's title and Open Recent follow too.
            opened.onMoved = { [weak self] url in
                guard let self, url != fileURL else { return }
                fileURL = url
            }
            model = opened
        } catch {
            if case let .success(core) = core, model == nil {
                // Opened, but no model came of it: let go of the core's
                // document (and its snapshot) off the main thread.
                core.release()
            }
            try fail(opening: error, url: url)
        }
    }

    /// An open failed: no rows will be drawn, so the signpost ends here.
    private func fail(opening error: any Error, url: URL) throws -> Never {
        if let opening {
            self.opening = nil
            Signposts.firstRows(opening)
        }
        throw Self.openError(error, url: url)
    }

    /// An error AppKit can present: the title, then the core's error in
    /// words (`OpenErrorText`).
    static func openError(_ error: any Error, url: URL) -> NSError {
        Logger.document.error("Couldn’t open \(url.path(percentEncoded: false), privacy: .private): \(String(describing: error), privacy: .public)")
        return NSError(
            domain: "io.github.robhaswell.leal",
            code: 1,
            userInfo: [
                NSLocalizedDescriptionKey: OpenErrorText.title(fileName: url.lastPathComponent),
                NSLocalizedRecoverySuggestionErrorKey: OpenErrorText.describe(error),
                NSUnderlyingErrorKey: error as NSError,
            ]
        )
    }

    /// **Reload** (task 1.9): the file's banner, File ▸ Reload from Disk,
    /// and a second `read(from:)` all come here. The model reads the file
    /// again, at `url` if given, and `NSDocument` is told the file Leal now
    /// shows (its URL and modification date), so its own check before a
    /// save (phase 2) doesn't take the change the user just accepted for
    /// one made by another app (phase 1 review, app-3).
    ///
    /// Throws the open error if the file can't be opened; the document is
    /// then unchanged.
    func reload(from url: URL? = nil) throws {
        guard let model else { return }
        if let failure = model.failure {
            // A failed document is reopened, not reloaded (DESIGN §3.9).
            throw failure
        }
        let target = url ?? model.url
        let modified = Self.modificationDate(of: target)
        try model.reload(from: target)
        followReload(of: model, modified: modified)
    }

    /// **Reload** of a file on a network share (ADR-0009): as `reload`, with
    /// everything that touches the file (its modification date, and the
    /// core's open) off the main thread. The window's Reload comes here for
    /// such a file.
    func reloadInBackground() async throws {
        guard let model else { return }
        if let failure = model.failure { throw failure }
        let target = model.url
        let modified = await FileWork.run { Self.modificationDate(of: target) }
        // Only if the new snapshot was adopted: one that wasn't (the
        // document failed or closed meanwhile) mustn't move NSDocument's
        // idea of the file.
        if try await model.reloadInBackground(from: target) {
            followReload(of: model, modified: modified)
        }
    }

    /// The file's modification date, read before the core's snapshot, so a
    /// change in between is still a change by the time Leal saves. From the
    /// file system, not the URL's cached resource values.
    nonisolated private static func modificationDate(of url: URL) -> Date? {
        let attributes = try? FileManager.default.attributesOfItem(atPath: url.path(percentEncoded: false))
        return attributes?[.modificationDate] as? Date
    }

    /// `NSDocument` is told the file Leal shows after a Reload.
    private func followReload(of model: DocumentModel, modified: Date?) {
        if fileURL != model.url {
            fileURL = model.url
        }
        fileModificationDate = modified
    }

    override func makeWindowControllers() {
        guard let model, let environment else { return }
        let controller = DocumentWindowController(model: model, scheduler: environment.scheduler)
        controller.content.onFailure = { [weak self] in self?.presentFailure() }
        controller.content.onReload = { [weak self] in try await self?.reloadInBackground() }
        watchWindow(controller.window)
        if let opening {
            self.opening = nil
            controller.content.grid.gridView.onFirstRows = { Signposts.firstRows(opening) }
        }
        addWindowController(controller)
        if model.isFailed {
            // Failed after opening, before there was a window: offer to
            // reopen once it's on screen (`showWindows`).
            presentFailure()
        } else {
            model.start()
        }
    }

    override func showWindows() {
        super.showWindows()
        // A failure found before the window was on screen.
        if model?.isFailed == true {
            presentFailure()
        }
        #if LEAL_BENCH
        ScriptedRun.documentShown(self)
        #endif
    }

    /// Phase 1 is a viewer: there is nothing to save yet.
    override func data(ofType typeName: String) throws -> Data {
        throw NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError)
    }

    /// Whether Save may write over the file: not once its removable drive
    /// was disconnected before Leal had read it all, once it changed while
    /// Leal read it, or while its drive isn't connected (ADR-0006, 1.1a,
    /// 1.9). Save As is always allowed. SEAM(2.5): Save asks before writing
    /// over a file that changed elsewhere (`model.original.diverged`, which
    /// stays true after Keep Editing; DESIGN §3.1).
    var canSave: Bool { model?.canSave ?? false }

    override func validateUserInterfaceItem(_ item: any NSValidatedUserInterfaceItem) -> Bool {
        if item.action == #selector(save(_:)), !canSave {
            return false
        }
        return super.validateUserInterfaceItem(item)
    }

    /// Save is refused while `canSave` is false (ADR-0006): the banner
    /// offers Save As instead.
    override func save(_ sender: Any?) {
        guard canSave else {
            NSSound.beep()
            return
        }
        super.save(sender)
    }

    override func close() {
        // The window's find search and copy go first (task 1.8): they hold
        // the core's document, and so its file, until they stop.
        for case let controller as DocumentWindowController in windowControllers {
            controller.content.documentWillClose()
        }
        for observer in windowObservers { NotificationCenter.default.removeObserver(observer) }
        windowObservers.removeAll()
        model?.close()
        super.close()
    }

    // MARK: After a failure (DESIGN §3.9)

    /// Whether the user can see `window`, so that a sheet on it is seen: a
    /// background tab can, a minimised window can't. Tests replace it.
    var isOnScreen: (NSWindow) -> Bool = { $0.isVisible && !$0.isMiniaturized }
    /// Shows `alert` as a sheet on `window`, then calls `done` with the
    /// button chosen. Tests replace it, so that no sheet is shown.
    var showSheet: (_ alert: NSAlert, _ window: NSWindow, _ done: @escaping @MainActor (NSApplication.ModalResponse) -> Void) -> Void = { alert, window, done in
        alert.beginSheetModal(for: window) { response in
            MainActor.assumeIsolated { done(response) }
        }
    }
    /// The window's notifications that bring a waiting failure alert up.
    private var windowObservers: [any NSObjectProtocol] = []

    /// The core panicked: the model has stopped calling it. Say so and
    /// offer to reopen the file, which makes a new core document. While the
    /// window is out of sight (minimised, or not on screen yet), the alert
    /// waits until it is back (`watchWindow`): a sheet on a hidden window
    /// would never be seen (phase 1 review, app-2).
    func presentFailure() {
        isOfferingReopen = true
        guard !failureShown, let window = windowControllers.first?.window, isOnScreen(window) else { return }
        failureShown = true
        showSheet(failureAlert(), window) { [weak self] response in
            if response == .alertFirstButtonReturn {
                self?.reopenAfterFailure(display: true) { _ in }
            } else {
                self?.close()
            }
        }
    }

    /// The alert after a failure: the file's name, why in words from the
    /// catalog (never the core's message), Reopen and Close.
    func failureAlert() -> NSAlert {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = String(
            localized: "Leal can’t show “\(displayName ?? "")” any more.",
            comment: "Alert title after a panic in the core (DESIGN §3.9); the file's name"
        )
        alert.informativeText = OpenErrorText.describe(model?.failure ?? LealError.Internal(message: ""))
        alert.addButton(withTitle: String(localized: "Reopen", comment: "Button: open the file again after a failure"))
        alert.addButton(withTitle: String(localized: "Close", comment: "Button: close the document after a failure"))
        return alert
    }

    /// Brings up a waiting failure alert when the window comes back into
    /// sight: it becomes key, is restored from the Dock, or is uncovered.
    private func watchWindow(_ window: NSWindow?) {
        guard let window, windowObservers.isEmpty else { return }
        let names = [
            NSWindow.didBecomeKeyNotification,
            NSWindow.didDeminiaturizeNotification,
            NSWindow.didChangeOcclusionStateNotification,
        ]
        for name in names {
            let observer = NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self, self.isOfferingReopen else { return }
                    self.presentFailure()
                }
            }
            windowObservers.append(observer)
        }
    }

    /// Closes this document and opens its file again, through the document
    /// controller (so the window, Open Recent and tabs behave as for any
    /// open).
    func reopenAfterFailure(display: Bool, completion: @escaping @MainActor (NSDocument?) -> Void) {
        guard let url = fileURL else {
            completion(nil)
            return
        }
        close()
        NSDocumentController.shared.openDocument(withContentsOf: url, display: display) { document, _, error in
            MainActor.assumeIsolated {
                if let error { Logger.document.error("Reopening failed: \(String(describing: error), privacy: .public)") }
                completion(document)
            }
        }
    }
}

/// The app's one scheduler and its temporary folders, made on first use.
@MainActor
enum AppEnvironment {
    private static var made: DocumentEnvironment?

    static func shared() throws -> DocumentEnvironment {
        if let made { return made }
        let environment = try DocumentEnvironment(scheduler: Scheduler(), temp: TemporaryFolders.locations())
        made = environment
        return environment
    }
}
