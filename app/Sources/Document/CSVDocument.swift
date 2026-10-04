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

    /// The edit history (task 2.5.2): the undo manager over the core's
    /// commands, the recovery journal and the change-count tokens. The
    /// window uses its undo manager (`DocumentWindowController`);
    /// `NSDocument`'s own is off, so the change count follows the core's
    /// dirty state (`updateDirtyState`), not the undo manager's steps.
    let history = EditHistory()
    /// What the last Recover changes did, for tests.
    private(set) var lastRecovery: ReplayReport?
    /// A step's undo or redo the core refused, dealt with once the undo
    /// manager is done (`stepEnded`).
    private var refusedStep: (command: EditCommand, direction: CommandDirection, refusal: EditRefusal)?
    /// Asks whether to save the changes before closing, in place of
    /// `NSDocument`'s alert. Only tests set it: `answer` says whether to
    /// close.
    var unsavedChangesPromptForTesting: ((_ answer: @escaping @MainActor (Bool) -> Void) -> Void)?

    override init() {
        super.init()
        hasUndoManager = false
        let manager = history.undoManager
        manager.willUndoOrRedo = { [weak self] in self?.commitEditing() ?? true }
        manager.didUndoOrRedo = { [weak self] in self?.stepEnded() }
        manager.isAvailable = { [weak self] in self?.canUndoNow ?? false }
    }

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
                // An editor still open would edit the file read again: it
                // closes, as the edits go anyway (Revert to Saved commits
                // first, `revertToSaved`).
                for case let controller as DocumentWindowController in windowControllers {
                    controller.content.discardEditing()
                }
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
            opened.onCommand = { [weak self] command, direction in self?.commandApplied(command, as: direction) }
            opened.onReadingChanged = { [weak self] change in self?.readingChanged(change) }
            history.reset(choices: opened.choices)
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

    /// **Save As UTF-8** (task 2.3): the core writes the UTF-8 copy at
    /// `url`, and the document then *is* that copy (ADR-0008 decision 1), as
    /// after Save As: the window reads it as a Reload would, and `NSDocument`
    /// follows it (its URL and modification date), so its title, proxy icon
    /// and recent documents name the copy. Returns whether it was saved: not
    /// if the document failed or closed meanwhile.
    ///
    /// SEAM(2.5): the core's document already reads the copy after the save
    /// (its rebase); task 2.5's after-a-save path adopts that reading
    /// instead of opening the copy again.
    ///
    /// Editing is off meanwhile (the window's `isReplacingDocument`); an
    /// edit made all the same, after `version` (by default, the edit
    /// version now), keeps the window on this file, with its edits.
    ///
    /// - Throws: the save's `SaveFailure`, the Reload's open error, or
    ///   `EditedDuringReload`.
    @discardableResult
    func saveAsUTF8(to url: URL, editVersion version: UInt64? = nil) async throws -> Bool {
        guard let model else { return false }
        let version = version ?? model.editVersion
        guard try await model.saveAsUTF8(to: url) != nil else { return false }
        let modified = await FileWork.run { Self.modificationDate(of: url) }
        guard try await model.reloadInBackground(from: url, editVersion: version) else { return false }
        followReload(of: model, modified: modified)
        return true
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
        controller.content.onSaveAsUTF8 = { [weak self] url, version in
            try await self?.saveAsUTF8(to: url, editVersion: version) ?? false
        }
        controller.content.confirmDiscardingEdits = { [weak self] answer in
            guard let self else { return answer(true) }
            confirmDiscardingEdits(answer)
        }
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
    /// A UTF-16 file can be edited, but Save is off for it (ADR-0013
    /// decision 1): Save As UTF-8 is the only way to save it.
    var canSave: Bool { (model?.canSave ?? false) && model?.isReadOnly == false }

    override func validateUserInterfaceItem(_ item: any NSValidatedUserInterfaceItem) -> Bool {
        if item.action == #selector(save(_:)), !canSave {
            return false
        }
        return super.validateUserInterfaceItem(item)
    }

    /// **Revert to Saved** commits an open edit first, so that it counts
    /// (the document asks before throwing it away), or, if the core
    /// refuses it, stops with the editor open, saying why. SEAM(2.5.3):
    /// Revert proper.
    override func revertToSaved(_ sender: Any?) {
        guard commitEditing() else { return NSSound.beep() }
        super.revertToSaved(sender)
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

    // MARK: Undo, the journal and dirty state (task 2.5.2)

    /// Commits an edit still open in the window (the in-cell editor or the
    /// inspector), so that it counts: before undo and redo, closing, and
    /// anything that asks about unsaved changes. Returns whether none is
    /// left open (the core refused it, and it stays open, saying why).
    @discardableResult
    func commitEditing() -> Bool {
        windowControllers
            .compactMap { ($0 as? DocumentWindowController)?.content }
            .reduce(true) { $1.commitEditing() && $0 }
    }

    /// Undo and redo are off once the document has failed (Recover changes
    /// replays the journal instead), while it recovers, and while a Reload
    /// or Save As UTF-8 replaces the file shown.
    private var canUndoNow: Bool {
        guard let model, !model.isFailed, !model.isRecovering, !model.isReloading else { return false }
        return windowControllers.allSatisfy { ($0 as? DocumentWindowController)?.content.isReplacingDocument != true }
    }

    /// Every command the core applied (an edit, an undo, a redo) comes
    /// here, through the model's `commandApplied`: it goes in the journal,
    /// its undo (or redo) is registered, and the change count follows the
    /// core's dirty state, with a token noted at the edit version.
    private func commandApplied(_ command: EditCommand, as direction: CommandDirection) {
        guard let model else { return }
        let version = model.editVersion
        history.record(command, as: direction, version: version, choices: model.choices)
        history.register(command, as: direction) { [weak self] command, direction in
            self?.applyStep(command, direction)
        }
        updateDirtyState()
        history.noteToken(changeCountToken(for: .saveOperation), version: version)
    }

    /// An undo or redo step: the core applies it, and it comes back
    /// through `commandApplied`. A refusal waits for the undo manager to
    /// finish (`stepEnded`).
    private func applyStep(_ command: EditCommand, _ direction: CommandDirection) {
        guard let model else { return }
        let outcome = direction == .undo ? model.undo(command) : model.redo(command)
        if case let .refused(refusal) = outcome {
            refusedStep = (command, direction, refusal)
            if HistoryText.isTemporary(refusal) {
                // Back where it came from, to try again: done while the
                // undo manager is still in this undo or redo.
                history.putBack(command, as: direction) { [weak self] command, direction in
                    self?.applyStep(command, direction)
                }
            }
        }
        // `.failed`: the document failed, and offers to recover the
        // journal's edits.
    }

    /// The undo manager finished an undo or a redo. If the core refused
    /// it for now (the file is still being read, a row can't be read, a
    /// save runs), the step was put back to try again (`applyStep`). If it
    /// refused it because the cells changed (a cell no longer holds what
    /// the command left there), the steps around it can't be trusted to
    /// apply either: the history is cleared, with the edits left as they
    /// are. Either way the user is told.
    private func stepEnded() {
        guard let (command, direction, refusal) = refusedStep else { return }
        refusedStep = nil
        let temporary = HistoryText.isTemporary(refusal)
        if !temporary {
            history.undoManager.removeAllActions()
        }
        NSSound.beep()
        guard let window = windowControllers.first?.window else { return }
        let alert = NSAlert()
        alert.messageText = HistoryText.undoRefused(EditHistory.actionName(for: command), undo: direction == .undo)
        alert.informativeText = temporary ? HistoryText.undoNotYetDetail(refusal) : HistoryText.undoRefusedDetail(refusal)
        showSheet(alert, window) { _ in }
    }

    /// The document is edited while some cell reads differently from the
    /// file (the core's `hasUnsavedEdits`, DESIGN §3.6), so an edit set
    /// back by hand, or undone, leaves it clean. The change count is
    /// `NSDocument`'s, moved by this alone, so a save's token (2.5.3) marks
    /// the edits up to its snapshot as saved and those after as not.
    func updateDirtyState() {
        guard let model, !model.isFailed else { return }
        if model.hasUnsavedEdits {
            updateChangeCount(.changeDone)
        } else if isDocumentEdited {
            updateChangeCount(.changeCleared)
        }
    }

    /// The file was read again (the model's `onReadingChanged`). The
    /// Header row keeps the edits and their history; a new split or a new
    /// core document has none, and the commands from before it no longer
    /// apply, so the history goes (docs/tasks/2.1.md).
    private func readingChanged(_ change: ReadingChange) {
        guard let model else { return }
        switch change {
        case .sameSplit:
            history.readingChanged(model.choices)
        case .newSplit, .replaced:
            history.reset(choices: model.choices)
            updateChangeCount(.changeCleared)
        }
    }

    /// Asks before a Reload throws the unsaved edits away (ADR-0008
    /// decision 4). `answer` hears whether to go on.
    func confirmDiscardingEdits(_ answer: @escaping @MainActor (Bool) -> Void) {
        guard let window = windowControllers.first?.window else { return answer(true) }
        let alert = NSAlert()
        alert.messageText = HistoryText.discardForReload(displayName ?? "")
        alert.informativeText = HistoryText.discardForReloadDetail
        let reload = alert.addButton(withTitle: HistoryText.reload)
        reload.hasDestructiveAction = true
        alert.addButton(withTitle: HistoryText.cancel)
        showSheet(alert, window) { response in answer(response == .alertFirstButtonReturn) }
    }

    /// Closing commits an open edit first, so it counts as an unsaved
    /// change, then asks as `NSDocument` does (Save, Don't Save, Cancel).
    /// SEAM(2.5.3): Save then saves through the core.
    override func canClose(withDelegate delegate: Any, shouldClose shouldCloseSelector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        guard commitEditing() else {
            // The core refused the open edit: it stays open, saying why.
            return Self.answer(delegate, shouldCloseSelector, document: self, shouldClose: false, contextInfo: contextInfo)
        }
        if isDocumentEdited, let prompt = unsavedChangesPromptForTesting {
            prompt { [weak self] close in
                guard let self else { return }
                Self.answer(delegate, shouldCloseSelector, document: self, shouldClose: close, contextInfo: contextInfo)
            }
            return
        }
        super.canClose(withDelegate: delegate, shouldClose: shouldCloseSelector, contextInfo: contextInfo)
    }

    /// Calls `canClose`'s delegate, as `NSDocument` does:
    /// `document:shouldClose:contextInfo:`.
    private static func answer(_ delegate: Any, _ selector: Selector?, document: NSDocument, shouldClose: Bool, contextInfo: UnsafeMutableRawPointer?) {
        guard let selector, let object = delegate as? NSObject, object.responds(to: selector) else { return }
        typealias Callback = @convention(c) (NSObject, Selector, NSDocument, Bool, UnsafeMutableRawPointer?) -> Void
        let callback = unsafeBitCast(object.method(for: selector), to: Callback.self)
        callback(object, selector, document, shouldClose, contextInfo)
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
        let recover = canRecover
        showSheet(failureAlert(), window) { [weak self] response in
            guard let self else { return }
            switch (recover, response) {
            case (true, .alertFirstButtonReturn):
                Task { await self.recoverChanges() }
            case (true, .alertSecondButtonReturn), (false, .alertFirstButtonReturn):
                reopenAfterFailure(display: true) { _ in }
            default:
                close()
            }
        }
    }

    /// Whether the failed document has edits in its journal for Recover
    /// changes to put back (ADR-0008 decision 5). The journal alone: the
    /// core's dirty state can't be asked once it has failed.
    var canRecover: Bool {
        !history.journal.isEmpty
    }

    /// The alert after a failure: the file's name, why in words from the
    /// catalog (never the core's message), Reopen and Close. With edits to
    /// recover, it says that Reopen and Close throw them away, and both
    /// are marked destructive.
    func failureAlert() -> NSAlert {
        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = String(
            localized: "Leal can’t show “\(displayName ?? "")” any more.",
            comment: "Alert title after a panic in the core (DESIGN §3.9); the file's name"
        )
        alert.informativeText = OpenErrorText.describe(model?.failure ?? LealError.Internal(message: ""))
        let recover = canRecover
        if recover {
            alert.informativeText += "\n\n" + HistoryText.recoverChangesDetail
            alert.addButton(withTitle: HistoryText.recoverChanges)
        }
        let reopen = alert.addButton(withTitle: String(localized: "Reopen", comment: "Button: open the file again after a failure"))
        let close = alert.addButton(withTitle: String(localized: "Close", comment: "Button: close the document after a failure"))
        reopen.hasDestructiveAction = recover
        close.hasDestructiveAction = recover
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

    /// **Recover changes** (ADR-0008 decision 5, DESIGN §3.6): the model
    /// opens the file afresh with the journal's choices and replays the
    /// journal into it, and the window carries on with the edits. The
    /// commands that applied are the new undo history (`EditHistory`). If
    /// the file changed since it was opened, or an edit no longer applies,
    /// Leal offers Save As of what it recovered and names the edits it
    /// couldn't put back. If the file can't be opened, the failure alert
    /// comes back.
    func recoverChanges() async {
        guard let model, model.isFailed, !model.isRecovering else { return }
        isOfferingReopen = false
        let journal = history.journal
        let header = history.choices?.header ?? model.interpretation.header
        // What Leal knew of the file before it failed.
        let changedBefore = model.original.state != .unchanged || model.changedOnDisk
        let known = fileModificationDate
        let report: ReplayReport?
        do {
            report = try await model.recover(history.replayCommands, choices: history.choices)
        } catch {
            Logger.document.error("Recover changes failed: \(String(describing: error), privacy: .public)")
            presentRecoveryFailure(error)
            return
        }
        guard let report, self.model === model, !model.isFailed else { return }
        failureShown = false
        lastRecovery = report
        // The history first, before anything waits: the document can be
        // edited again from here, and an edit made while the date below is
        // read must land in the rebuilt history, not be dropped by it.
        history.rebuild(from: report, version: model.editVersion, choices: model.choices) { [weak self] command, direction in
            self?.applyStep(command, direction)
        }
        updateDirtyState()
        history.noteToken(changeCountToken(for: .saveOperation), version: model.editVersion)
        let url = model.url
        let modified = await FileWork.run { Self.modificationDate(of: url) }
        let changed = changedBefore || (known != nil && modified != known)
        guard changed || !report.refused.isEmpty, let window = windowControllers.first?.window else { return }
        let refused = report.refused.compactMap { item -> (command: EditCommand, refusal: EditRefusal)? in
            let index = Int(item.index)
            return index < journal.count ? (journal[index].applied, item.refusal) : nil
        }
        // Counted in edits: an undo or a redo isn't one of the user's
        // changes. An edit is put back when it applied.
        let refusedIndexes = Set(report.refused.map { Int($0.index) })
        let edits = journal.indices.filter { journal[$0].direction == .edit }
        let appliedEdits = edits.filter { !refusedIndexes.contains($0) }.count
        let alert = NSAlert()
        alert.messageText = HistoryText.recovered(displayName ?? "", applied: appliedEdits, total: edits.count)
        alert.informativeText = refused.isEmpty
            ? HistoryText.recoveredDetail
            : HistoryText.refused(refused, header: header) + "\n\n" + HistoryText.recoveredDetail
        alert.addButton(withTitle: HistoryText.saveAs)
        alert.addButton(withTitle: HistoryText.notNow)
        showSheet(alert, window) { [weak self] response in
            // SEAM(2.5.3): Save As saves through the core.
            if response == .alertFirstButtonReturn { self?.saveAs(nil) }
        }
    }

    /// Recover changes couldn't open the file: say why, then offer the
    /// failure's choices again.
    private func presentRecoveryFailure(_ error: any Error) {
        isOfferingReopen = true
        guard let window = windowControllers.first?.window else { return }
        let alert = NSAlert()
        alert.messageText = HistoryText.recoveryFailed(displayName ?? "")
        alert.informativeText = if case RecoveryError.readingDoesNotFit = error {
            HistoryText.recoveryReadingDoesNotFit
        } else {
            OpenErrorText.describe(error)
        }
        showSheet(alert, window) { [weak self] _ in
            guard let self else { return }
            failureShown = false
            presentFailure()
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
