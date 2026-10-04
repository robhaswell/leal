import AppKit
import QuartzCore
import UniformTypeIdentifiers
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
    /// A step's undo or redo the core refused, with the step's name,
    /// dealt with once the undo manager is done (`stepEnded`).
    private var refusedStep: (direction: CommandDirection, refusal: EditRefusal, name: String)?
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
                // A second read, as AppKit's `revert(toContentsOf:ofType:)`
                // would do: the window keeps its model, which reads the file
                // again, so it is never left bound to a closed one (phase 1
                // review, app-3). Leal's Revert to Saved never comes here
                // (task 2.5.3c): it reloads off the main thread, asking
                // first (`revertToSaved`, `revert(toContentsOf:ofType:)`).
                Logger.document.fault("A second read(from:) on the main thread")
                // An editor still open would edit the file read again: it
                // closes, as the edits go anyway.
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
            opened.onSavedReadingReviewed = { [weak self, weak opened] in
                guard let self, let opened else { return }
                rememberReviewedInterpretation(model: opened)
            }
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
    ///
    /// `editVersion`: the edit version the caller checked the edits at (by
    /// default, the one when the model's Reload starts): an edit made since
    /// stops the new reading being adopted (`EditedDuringReload`).
    func reloadInBackground(editVersion: UInt64? = nil) async throws {
        guard let model else { return }
        if let failure = model.failure { throw failure }
        let target = model.url
        let modified = await FileWork.run { Self.modificationDate(of: target) }
        // Only if the new snapshot was adopted: one that wasn't (the
        // document failed or closed meanwhile) mustn't move NSDocument's
        // idea of the file.
        if try await model.reloadInBackground(from: target, editVersion: editVersion) {
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

    /// **Save As UTF-8** (task 2.3, ADR-0008 decision 7): a Save As
    /// (`saveAsNow`) that writes the copy at `url` in UTF-8, and the
    /// document then *is* that copy (ADR-0008 decision 1): the window shows
    /// the reading the core made of it (task 2.5.3c), in UTF-8, with any
    /// edit made during the save carried over, and `NSDocument` follows it,
    /// so its title, proxy icon and recent documents name the copy. Its
    /// progress shows in the status bar, as Save's does. It waits for a
    /// Save under way, and cancelling the waiting task cancels it. Returns
    /// whether it was saved: not if the document failed or closed
    /// meanwhile.
    ///
    /// - Throws: the save's `SaveFailure`.
    @discardableResult
    func saveAsUTF8(to url: URL) async throws -> Bool {
        let result = await saveAsQueued(url, kind: .saveAsUtf8)
        return try result.get() != nil
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
        controller.content.onSaveAsUTF8 = { [weak self] url in
            try await self?.saveAsUTF8(to: url) ?? false
        }
        controller.content.onSaveAs = { [weak self] in self?.chooseSaveAs(.saveAs) }
        controller.content.onCancelSave = { [weak self] in self?.cancelSave() ?? false }
        controller.content.documentIsSaving = { [weak self] in self?.saving != nil }
        controller.content.undoHistory = { [weak self] in self?.history.undoManager }
        controller.content.confirmDiscardingEdits = { [weak self] reason, answer in
            guard let self else { return answer(true) }
            confirmDiscardingEdits(reason) { [weak self] proceed in
                // Not if a Save was queued while the question showed: it
                // isn't the model's yet (`isSaving`), but it will write.
                let queued = self?.saving != nil
                if proceed, queued { NSSound.beep() }
                answer(proceed && !queued)
            }
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

    /// Leal never writes through `NSDocument`'s own writing: Save goes
    /// through the core (`save(withDelegate:didSave:contextInfo:)`), and so
    /// do Save As and Duplicate (`saveAs(to:)`).
    override func data(ofType typeName: String) throws -> Data {
        throw NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError)
    }

    /// Another app's coordinated read asks the file's presenters to save
    /// first (`NSFilePresenter`), which `NSDocument` does by autosaving,
    /// through its own writing. Leal writes the file only when the user
    /// saves (DESIGN §4.3): there is nothing to save, so the reader goes
    /// on at once and reads the file as it is on disk.
    nonisolated override func savePresentedItemChanges(completionHandler: @escaping ((any Error)?) -> Void) {
        completionHandler(nil)
    }

    /// No autosave of any kind (`autosavesInPlace` is false): with no
    /// autosaving type, `NSDocument` never writes the edits elsewhere
    /// either.
    override var autosavingFileType: String? { nil }

    /// Whether Save may write over the file: not once its removable drive
    /// was disconnected before Leal had read it all, once it changed while
    /// Leal read it, or while its drive isn't connected (ADR-0006, 1.1a,
    /// 1.9). Save As is always allowed. A file that changed elsewhere can
    /// be saved, after asking (`saveInPlace`).
    /// A UTF-16 file can be edited, but Save is off for it (ADR-0013
    /// decision 1): Save As UTF-8 is the only way to save it.
    var canSave: Bool { (model?.canSave ?? false) && model?.isReadOnly == false }

    /// Revert to Saved only (ADR-0008 decision 10): no Versions browser,
    /// which needs autosave in place.
    nonisolated override class var preservesVersions: Bool { false }

    override func validateUserInterfaceItem(_ item: any NSValidatedUserInterfaceItem) -> Bool {
        switch item.action {
        case #selector(save(_:)) where !canSave || saving != nil || isRereading:
            return false
        case #selector(saveTo(_:)):
            // No Export: it would write through `NSDocument`.
            return false
        case #selector(saveAs(_:)), #selector(duplicate(_:)), #selector(saveDuplicate(_:)):
            // Not while a Reload or Revert reads the file again: the
            // document would follow the copy while the window adopted the
            // old file's new reading (task 2.5.3c review).
            if model == nil || model?.isFailed == true || isRereading { return false }
        case #selector(revertToSaved(_:)):
            // Not while the file is read again already.
            if model == nil || model?.isFailed == true || model?.isReloading == true { return false }
        default:
            break
        }
        if saving != nil, let action = item.action, Self.waitForSave.contains(action) {
            return false
        }
        return super.validateUserInterfaceItem(item)
    }

    /// A Reload or Revert to Saved is reading the file again: the model's,
    /// or the window's, which is set a moment before the model's starts.
    /// Save, Save As and Duplicate are off meanwhile.
    var isRereading: Bool {
        model?.isReloading == true || (windowControllers.first as? DocumentWindowController)?.content.reloading != nil
    }

    /// What waits for a Save under way (task 2.5.3a review): each would
    /// read the file again, write it, or move, rename or lock it while the
    /// save replaces it, and so mustn't run before `NSDocument` and the
    /// model have heard what the save did.
    private static let waitForSave: [Selector] = [
        #selector(revertToSaved(_:)), #selector(saveAs(_:)), #selector(saveTo(_:)), #selector(duplicate(_:)), #selector(saveDuplicate(_:)),
        #selector(move(_:)), #selector(rename(_:)), #selector(lock(_:)), #selector(NSDocument.unlock(_:)),
    ]

    /// **Revert to Saved** (ADR-0008 decision 4, DESIGN §4.3) goes the way
    /// File ▸ Reload from Disk does, never through AppKit's alert and its
    /// `read(from:)` on the main thread, where a share must never be read
    /// (ADR-0010): it commits an open edit first, so that it counts (or,
    /// if the core refuses it, stops with the editor open, saying why),
    /// asks before discarding unsaved edits, then reads the file again off
    /// the main thread (`reloadInBackground`). It waits for a Save under
    /// way (`waitForSave`).
    override func revertToSaved(_ sender: Any?) {
        guard saving == nil, let content = (windowControllers.first as? DocumentWindowController)?.content else {
            return NSSound.beep()
        }
        content.reload(.revert)
    }

    /// AppKit's own revert reads the file on the main thread
    /// (`read(from:)`): Leal reads it again off the main thread instead, as
    /// Reload does. Whoever calls this has asked already; it never discards
    /// unsaved edits, though (an edited document is left as it is).
    override func revert(toContentsOf url: URL, ofType typeName: String) throws {
        guard let model, !model.isFailed, url.standardizedFileURL == model.url.standardizedFileURL else {
            throw NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError)
        }
        guard !model.hasUnsavedEdits, saving == nil, !isRereading else {
            Logger.document.error("Not reverted: there are unsaved edits, or the file is being saved or read")
            return
        }
        // The edit version now, with the check above: an edit made before
        // the reading is adopted stops it (`EditedDuringReload`), so it is
        // never thrown away unasked (task 2.5.3c review).
        let version = model.editVersion
        // Reading again from now, not from when the task starts: Save, Save
        // As and Duplicate are off at once (`isRereading`). Every way out
        // of the task ends it.
        model.willReload()
        Task { [weak self] in
            defer { model.reloadEnded() }
            do {
                try await self?.reloadInBackground(editVersion: version)
            } catch {
                Logger.document.error("Revert failed: \(String(describing: error), privacy: .public)")
            }
        }
    }

    /// Save is refused while `canSave` is false (ADR-0006): the banner
    /// offers Save As instead. AppKit then calls
    /// `save(withDelegate:didSave:contextInfo:)`.
    override func save(_ sender: Any?) {
        guard canSave else {
            NSSound.beep()
            return
        }
        super.save(sender)
    }

    // MARK: Save (task 2.5.3a, ADR-0012)

    /// The Save under way, and whether it saved: for tests, and so that
    /// a second one waits for it. While it is set, the document stays
    /// edited (`updateDirtyState`), closing and quitting wait for it
    /// (`waitForSaves`), and Revert, Save As, Duplicate, Move, Rename and
    /// Lock are off (`waitForSave`).
    private(set) var saving: Task<Bool, Never>?
    /// Every Save not yet ended, the one running and those waiting for
    /// it, by number, for `cancelSave`.
    private var saves: [Int: Task<Bool, Never>] = [:]
    /// The last Save's number.
    private var saveNumber = 0
    /// Who waits for `saving` to end (`waitForSaves`).
    private var saveWaiters: [CheckedContinuation<Void, Never>] = []
    /// Why the last Save to end didn't write the file, or `nil` if it did
    /// (or none has run): for the scripted save check and tests.
    private(set) var lastSaveFailure: SaveFailure?
    /// What the last Save to write the file did (whether it swapped the
    /// files and checked the old one, any old file kept): for the
    /// scripted save check.
    private(set) var lastSaveOutcome: SaveOutcome?
    /// What a Save alert's Duplicate, Save As… and Save As UTF-8… do, in
    /// place of doing it. Only tests set it.
    var saveFollowUpForTesting: ((SaveChoice) -> Void)?
    /// How many times Save tries again on its own when the file is in the
    /// middle of a rename (`Moving`), and how long it waits each time:
    /// together longer than the core's `MOVE_WINDOW` (2 s), after which a
    /// rename is a move, not part of another app's save.
    static let movingRetries = 10
    static let movingDelay: Duration = .milliseconds(250)

    /// **Save** (ADR-0012, DESIGN §4.3): every Save comes here, ⌘S and
    /// closing's Save alike, in place of `NSDocument`'s own writing and its
    /// own checks, so the user sees Leal's prompts only (one about a file
    /// changed elsewhere, never `NSDocument`'s too). An open edit is
    /// committed first. Saves run one after another; `didSaveSelector`
    /// hears whether this one saved. A document without a file (none
    /// today) goes the usual way, through a save panel.
    override func save(withDelegate delegate: Any?, didSave didSaveSelector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        guard fileURL != nil, model != nil else {
            return super.save(withDelegate: delegate, didSave: didSaveSelector, contextInfo: contextInfo)
        }
        let reply: @MainActor (Bool) -> Void = { [weak self] saved in
            guard let self else { return }
            Self.answer(delegate, didSaveSelector, document: self, flag: saved, contextInfo: contextInfo)
        }
        guard !isRereading, commitEditing() else {
            // A Reload or Revert is reading the file again, or the core
            // refused the open edit (it stays open, saying why).
            NSSound.beep()
            return reply(false)
        }
        let task = enqueueSave { await $0.saveInPlace() }
        Task {
            reply(await task.value)
        }
    }

    /// Queues a Save, Save As or Save As UTF-8 (`body`) behind those under
    /// way: they run one after another, each a `saving` that closing,
    /// quitting and ⌘. know of. Returns its task, whose value says whether
    /// it saved.
    @discardableResult
    private func enqueueSave(_ body: @escaping @MainActor (CSVDocument) async -> Bool) -> Task<Bool, Never> {
        let previous = saving
        saveNumber += 1
        let number = saveNumber
        let task = Task { [weak self] () -> Bool in
            _ = await previous?.value
            var saved = false
            if let document = self { saved = await body(document) }
            // Before the task's value is known: whoever awaits it finds
            // `saving` and the dirty state up to date.
            self?.saveEnded(number)
            return saved
        }
        saving = task
        saves[number] = task
        return task
    }

    /// Save `number` has ended.
    private func saveEnded(_ number: Int) {
        saves[number] = nil
        if saves.isEmpty { savesEnded() }
    }

    /// Every Save has ended: the dirty state is the core's again (an edit
    /// set back by hand during the save is clean now), and whoever waits
    /// for the saves (closing, quitting) goes on.
    private func savesEnded() {
        saving = nil
        updateDirtyState()
        let waiters = saveWaiters
        saveWaiters.removeAll()
        for waiter in waiters { waiter.resume() }
    }

    /// Returns once no Save is under way (at once if none is), without
    /// blocking the main thread. Closing and quitting wait here before they
    /// decide whether to ask about unsaved changes.
    func waitForSaves() async {
        while saving != nil {
            await withCheckedContinuation { saveWaiters.append($0) }
        }
    }

    /// Stops the Saves under way (⌘. or Escape while one runs or waits to
    /// start): the one running and any waiting for it. Before the new file
    /// is in place nothing is written, and the edits stay unsaved. Returns
    /// whether there was one to stop.
    @discardableResult
    func cancelSave() -> Bool {
        guard saving != nil else { return false }
        for save in saves.values { save.cancel() }
        return true
    }

    /// Save, then: asks first if the file changed elsewhere, saves through
    /// the core, and says why if it couldn't, offering what the user can
    /// do. Returns whether it saved.
    private func saveInPlace() async -> Bool {
        guard let model, !model.isFailed else { return false }
        guard canSave else {
            let failure: SaveFailure = if model.isReadOnly {
                .ReadOnly
            } else if model.original.state == .unavailable {
                .Unavailable
            } else {
                .Incomplete
            }
            lastSaveFailure = failure
            await refuse(failure, model: model)
            return false
        }
        // The watcher saw a change (it stays seen after Keep Editing). Not
        // for a file deleted since: the core says it's missing, and a new
        // file there is a change it asks about.
        var overwrite = false
        if model.original.diverged, model.original.state != .deleted {
            guard await ask(SaveText.changedElsewhere(name: fileName)) == .saveAnyway else { return false }
            overwrite = true
        }
        var moves = 0
        while !Task.isCancelled {
            guard let url = fileURL else { return false }
            let result = await saveCoordinated(url, kind: .save, overwriteChanged: overwrite, model: model)
            // Save Anyway's consent is for the file the user was asked
            // about: any other way round the loop asks again.
            overwrite = false
            switch result {
            case let .success(saved?):
                lastSaveFailure = nil
                lastSaveOutcome = saved.outcome
                // The user hears of a kept old file before the save ends,
                // so Save and Close, and Quit, wait for it.
                if let reporting = saveFinished(saved, kind: .save, model: model) {
                    _ = await reporting.value
                }
                return true
            case .success(nil):
                // The document failed or closed meanwhile.
                return false
            case var .failure(failure):
                lastSaveFailure = failure
                Self.log(failure, url: url)
                if case .ChangedElsewhere = failure, await FileWork.run({ !Self.exists(url) }) {
                    // The watcher saw it deleted: say it's missing, rather
                    // than ask to write over it and then say so.
                    failure = .Missing
                }
                if case .Moving = failure, moves < Self.movingRetries {
                    // Another app's save renaming the file a moment ago:
                    // try again shortly (DESIGN §3.7).
                    moves += 1
                    model.waitingToSave(true)
                    try? await Task.sleep(for: Self.movingDelay)
                    model.waitingToSave(false)
                    continue
                }
                if case .Locked = failure, await FileWork.run({ Self.isLockedBySystem(url) }) {
                    // Only an administrator can clear `schg`: no Unlock.
                    if await ask(SaveText.lockedBySystem(name: fileName)) == .duplicate { followUp(.duplicate) }
                    return false
                }
                switch await refuse(failure, model: model) {
                case .saveAnyway:
                    overwrite = true
                case .tryAgain:
                    moves = 0
                case .unlock:
                    let unlocked = await FileWork.run { Self.unlock(url) }
                    if !unlocked, await ask(SaveText.unlockFailed(name: fileName)) == .duplicate {
                        followUp(.duplicate)
                        return false
                    }
                    if !unlocked { return false }
                default:
                    return false
                }
            }
        }
        return false
    }

    /// The file's name, as the window's title has it.
    private var fileName: String {
        displayName ?? fileURL?.lastPathComponent ?? ""
    }

    /// Logs why a Save failed, at error level: for a write that failed, the
    /// step, the OS error (`errno`) and the file, so the cause can be found
    /// in Console from the user's report.
    private static func log(_ failure: SaveFailure, url: URL, what: String = "Save") {
        let path = url.path(percentEncoded: false)
        if case let .Io(step, code, message) = failure {
            let errno = code.map { String($0) } ?? "none"
            Logger.document.error(
                "\(what, privacy: .public) failed \(step, privacy: .public) (errno \(errno, privacy: .public): \(message, privacy: .public)) for \(path, privacy: .private)"
            )
        } else {
            Logger.document.error("\(what, privacy: .public) failed: \(String(describing: failure), privacy: .public) for \(path, privacy: .private)")
        }
    }

    /// The save itself, as one of the document's file accesses
    /// (`performAsynchronousFileAccess`, so it never overlaps another),
    /// inside a coordinated write that replaces the file (`.forReplacing`,
    /// the document as file presenter: other apps and iCloud Drive get
    /// notice, and `NSDocument` doesn't react to its own save). The core
    /// writes on a thread of its own; nothing here blocks the main thread.
    /// `nil` if the document failed or closed.
    ///
    /// The status bar says "Waiting to save…" until the core's save starts.
    ///
    /// Save As (`kind` not `.save`) is the same, with the coordinated write
    /// on the new file: `url` is where it goes.
    private func saveCoordinated(_ url: URL, kind: SaveKind, overwriteChanged: Bool, model: DocumentModel) async -> Result<DocumentModel.Saved?, SaveFailure> {
        model.waitingToSave(true)
        defer { model.waitingToSave(false) }
        return await Self.write(url, kind: kind, overwriteChanged: overwriteChanged, document: self, model: model)
    }

    /// `saveCoordinated`, off the main thread from the moment the document
    /// has its file access until it ends it (task 2.5.3a review).
    ///
    /// While the access lasts, `NSDocument` waits for it, blocking the main
    /// thread, wherever it reads its own `fileURL`, `isDocumentEdited` or
    /// `fileModificationDate` (it does so inside
    /// `performSynchronousFileAccess`). So nothing between the access
    /// starting and ending may need the main thread, or it could wait for
    /// a main thread that waits for it: no `await` here resumes on the main
    /// actor. The work that must be on the main thread (starting the core's
    /// save, and `NSDocument`'s bookkeeping once it has written the file)
    /// goes through `continueAsynchronousWorkOnMainThread` (`onMain`),
    /// which `NSDocument` lets in even while it waits. The coordinated write
    /// and the access end here, off the main thread, as soon as the core's
    /// save is over.
    nonisolated private static func write(
        _ url: URL,
        kind: SaveKind,
        overwriteChanged: Bool,
        document: CSVDocument,
        model: DocumentModel
    ) async -> Result<DocumentModel.Saved?, SaveFailure> {
        guard let access = await document.fileAccess() else { return .failure(.Cancelled) }
        defer { access.done() }
        if Task.isCancelled { return .failure(.Cancelled) }
        let write: CoordinatedWrite
        do {
            write = try await CoordinatedWrite.begin(replacing: url, presenter: document)
        } catch is CancellationError {
            return .failure(.Cancelled)
        } catch {
            Logger.document.error("Couldn’t coordinate the save: \(String(describing: error), privacy: .public)")
            return .failure(.Io(step: "coordinating the save", code: nil, message: String(describing: error)))
        }
        defer { write.end() }
        // Cancelled while waiting, but given access all the same.
        if Task.isCancelled { return .failure(.Cancelled) }
        let target = write.url
        let place = await DocumentModel.placeForSaving(target)
        let (started, handle) = await document.onMain {
            model.waitingToSave(false)
            return (model.startSave(to: target, kind: kind, place: place, overwriteChanged: overwriteChanged), model.readingID.handle)
        }
        guard case let .success(job?) = started else {
            place.removeLeftovers()
            return started.map { _ -> DocumentModel.Saved? in nil }
        }
        let result = await DocumentModel.outcome(of: job, handle: handle)
        await document.onMain {
            // A save that wrote the file goes on to `saveFinished`, and so
            // to `DocumentModel.saved`.
            model.saveEnded(job, place: place, outcomeFollows: (try? result.get()) != nil)
            if case let .success(saved) = result {
                document.noteWritten(saved, kind: kind, model: model)
            }
        }
        return result.map { Optional($0) }
    }

    /// Runs `body` on the main thread, even while `NSDocument` blocks it
    /// waiting for this document's file access
    /// (`continueAsynchronousWorkOnMainThread`), and returns what it
    /// returned. For work done during the save's file access.
    nonisolated private func onMain<T: Sendable>(_ body: @escaping @MainActor @Sendable () -> T) async -> T {
        await withCheckedContinuation { continuation in
            continueAsynchronousWorkOnMainThread {
                MainActor.assumeIsolated {
                    continuation.resume(returning: body())
                }
            }
        }
    }

    /// Waits for the document's turn at its file, and returns what ends it.
    /// The access is asked for on the main thread, as `NSDocument` needs,
    /// but the wait resumes off it: once the access has begun, the main
    /// thread may be blocked waiting for it to end.
    ///
    /// Cancelling the waiting task stops the wait at once (another app may
    /// be writing the file, which `NSDocument` waits for): it returns `nil`,
    /// and the access is ended as soon as it begins.
    nonisolated private func fileAccess() async -> FileAccess? {
        let waiting = AccessWait()
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                waiting.wait(continuation)
                DispatchQueue.main.async {
                    self.performAsynchronousFileAccess { done in
                        waiting.begin(FileAccess(done: done))
                    }
                }
            }
        } onCancel: {
            waiting.cancel()
        }
    }

    /// The end of one of `NSDocument`'s asynchronous file accesses. It may
    /// be called from any thread.
    private struct FileAccess: @unchecked Sendable {
        let done: () -> Void
    }

    /// A wait for a file access that may be cancelled (`fileAccess()`):
    /// whichever comes first, the access or the cancel, resumes the
    /// waiting task; an access that begins after a cancel ends at once.
    private final class AccessWait: @unchecked Sendable {
        private let lock = NSLock()
        private var continuation: CheckedContinuation<FileAccess?, Never>?
        private var cancelled = false

        func wait(_ continuation: CheckedContinuation<FileAccess?, Never>) {
            lock.lock()
            if cancelled {
                lock.unlock()
                continuation.resume(returning: nil)
            } else {
                self.continuation = continuation
                lock.unlock()
            }
        }

        func begin(_ access: FileAccess) {
            lock.lock()
            let waiting = continuation
            continuation = nil
            lock.unlock()
            if let waiting {
                waiting.resume(returning: access)
            } else {
                access.done()
            }
        }

        func cancel() {
            lock.lock()
            cancelled = true
            let waiting = continuation
            continuation = nil
            lock.unlock()
            waiting?.resume(returning: nil)
        }
    }

    /// The core wrote the file: `NSDocument` hears what it now has on disk,
    /// while the save's file access still lasts, as its own saving does
    /// (`continueAsynchronousWorkOnMainThread`): its modification date, so
    /// its own "changed by another application" check doesn't fire, and
    /// the change-count token noted at the save's snapshot, so the edits
    /// up to it are saved. Nothing, if the document now shows another
    /// model.
    ///
    /// After a Save As the document is the new file (ADR-0008 decision
    /// 1): its URL (at the name on disk the core reports) and type, so
    /// the title and proxy icon name it, as `NSDocument`'s own Save As
    /// does inside its file access.
    ///
    /// Nothing, too, if a Reload replaced the core document the save was
    /// started on (`isCurrent`).
    private func noteWritten(_ saved: DocumentModel.Saved, kind: SaveKind, model: DocumentModel) {
        guard self.model === model, !model.isFailed, Self.isCurrent(saved, model: model) else { return }
        // Onto its own file (a link, another case), the document keeps its
        // name: the core reports where a link leads.
        let renamed = kind != .save && !saved.outcome.ontoOpenFile
        if renamed {
            let url = URL(filePath: saved.outcome.path)
            if fileURL != url { fileURL = url }
            if let type = Self.fileType(for: url) { fileType = type }
        }
        fileModificationDate = saved.outcome.modified
        if let version = saved.snapshotVersion, let token = history.token(atVersion: version) {
            updateChangeCount(withToken: token, for: renamed ? .saveAsOperation : .saveOperation)
        }
    }

    /// Whether `saved` is about the core document `model` shows: not if a
    /// Reload or Revert replaced it while the save ran (task 2.5.3c
    /// review). The window then shows the file the Reload read, and a Save
    /// As it ran isn't followed: the document stays where it was, and the
    /// copy written is left as it is. Reload and Revert wait for every save,
    /// queued ones too (`documentIsSaving`, `waitForSave`), so this is a
    /// guard only, and the log line is all it says.
    private static func isCurrent(_ saved: DocumentModel.Saved, model: DocumentModel) -> Bool {
        guard saved.handle == model.readingID.handle else {
            Logger.document.error("A save ended after a Reload replaced the document it saved: not followed")
            return false
        }
        return true
    }

    /// The document type of a file named `url`: TSV or CSV by its
    /// extension, or `nil` to keep the one it has.
    nonisolated static func fileType(for url: URL) -> String? {
        guard let type = UTType(filenameExtension: url.pathExtension) else { return nil }
        if type.conforms(to: .tabSeparatedText) { return UTType.tabSeparatedText.identifier }
        if type.conforms(to: .commaSeparatedText) { return UTType.commaSeparatedText.identifier }
        return nil
    }

    /// A save wrote the file (`noteWritten` told `NSDocument`): the
    /// journal keeps only the commands after the save's snapshot, which a
    /// replay into the saved file still applies, and the model hears the
    /// file's new status. Edits made during the save stay unsaved: the
    /// dirty state is the core's again once every Save has ended
    /// (`savesEnded`). Nothing, if the document now shows another model,
    /// except that a kept old file is always reported. Returns the report
    /// of a kept old file (`keepOldFile`), for the save to wait for.
    ///
    /// After a Save As the model follows the new file too, the journal
    /// replays into it with the reading's choices (Save As UTF-8 reads
    /// UTF-8), and it goes in Open Recent.
    private func saveFinished(_ saved: DocumentModel.Saved, kind: SaveKind, model: DocumentModel) -> Task<URL?, Never>? {
        let outcome = saved.outcome
        if let kept = outcome.keptOldFile {
            keptOldFile = Task { await keepOldFile(kept) }
        }
        let reporting = outcome.keptOldFile == nil ? nil : keptOldFile
        guard self.model === model, !model.isFailed else { return reporting }
        if !outcome.skippedMetadata.isEmpty {
            Logger.document.info("Saved without some of the file’s metadata: \(outcome.skippedMetadata.joined(separator: ", "), privacy: .public)")
        }
        if let error = outcome.rereadError {
            Logger.document.error("Saved, but couldn’t read the file back: \(error, privacy: .public)")
        }
        guard saved.handle == model.readingID.handle else {
            // Read again under the save: only the outcome's end is noted.
            model.saved(saved, kind: kind)
            return reporting
        }
        if let version = saved.snapshotVersion {
            history.savedThrough(version: version)
        }
        model.saved(saved, kind: kind)
        if kind != .save {
            history.readingChanged(model.choices)
            Self.noteRecentDocument(model.url)
        }
        return reporting
    }

    // MARK: After a save (task 2.5.3b)

    /// Moving the last kept old file and telling the user (`keepOldFile`),
    /// for tests: where it is now, or `nil` if it couldn't be moved.
    private(set) var keptOldFile: Task<URL?, Never>?
    /// The last recording of the interpretation attribute after a save's
    /// review (`rememberReviewedInterpretation`), for tests: whether it
    /// wrote it.
    private(set) var remembering: Task<Bool, Never>?

    /// A save kept the old file at `path` (`SaveOutcome.keptOldFile`): it
    /// may hold another app's version. The core's first choice is next to
    /// the file saved (`a (replaced, kept by Leal).csv`), where it stays.
    /// Otherwise it is in the save's temporary folder, which the system may
    /// empty, so it is moved at once to Leal's Recovered folder
    /// (`RecoveredFiles`), off the main thread. Either way the user is told
    /// where it is, with Show in Finder; if it can't be moved, where it is
    /// instead. The save waits for the user's answer (`saveInPlace`), so
    /// Save and Close and Quit tell the user before the window goes. With
    /// no window, the alert is app-modal (`showAlert`). Returns where it is
    /// now, or `nil` if it couldn't be moved.
    @discardableResult
    func keepOldFile(_ path: String) async -> URL? {
        let name = fileName
        let file = fileURL
        let kept = await FileWork.run(qos: .utility) { RecoveredFiles.place(path, savedTo: file) }
        let message: SaveText.Message
        let shown: URL
        switch kept {
        case let .besideFile(url):
            Logger.document.error("Saved; the old file was kept next to it, as \(url.path(percentEncoded: false), privacy: .private)")
            message = SaveText.keptOldFileBeside(name: name, keptAs: url.lastPathComponent)
            shown = url
        case let .moved(url):
            Logger.document.error("Saved; the old file was kept and moved to \(url.path(percentEncoded: false), privacy: .private)")
            message = SaveText.keptOldFile(name: name, keptAs: url.lastPathComponent)
            shown = url
        case let .notMoved(error):
            Logger.document.error("Saved; the old file was kept at \(path, privacy: .private) and couldn’t be moved: \(String(describing: error), privacy: .public)")
            message = SaveText.keptOldFileNotMoved(name: name, path: path)
            shown = URL(filePath: path)
        }
        let alert = NSAlert()
        alert.messageText = message.title
        alert.informativeText = message.detail
        alert.addButton(withTitle: SaveText.button(.ok))
        alert.addButton(withTitle: SaveText.showInFinder)
        let window = windowControllers.first?.window
        let response = await withCheckedContinuation { continuation in
            // A sheet on a window nobody can see (minimised, or the app
            // hidden) would never be answered, and the save, and a quit
            // or close waiting on it, would hang.
            if let window, isOnScreen(window) {
                showSheet(alert, window) { continuation.resume(returning: $0) }
            } else {
                showAlert(alert) { continuation.resume(returning: $0) }
            }
        }
        if response == .alertSecondButtonReturn {
            NSWorkspace.shared.activateFileViewerSelecting([shown])
        }
        switch kept {
        case let .besideFile(url), let .moved(url): return url
        case .notMoved: return nil
        }
    }

    /// Shows `alert` app-modally, for a document with no window (closed by
    /// Save and Close, or by Quit), then calls `done` with the button
    /// chosen. Tests replace it, so that no alert is shown.
    var showAlert: (_ alert: NSAlert, _ done: @escaping @MainActor (NSApplication.ModalResponse) -> Void) -> Void = { alert, done in
        done(alert.runModal())
    }

    /// The review of a save's reading suggests another delimiter (ADR-0008
    /// decision 8's whole-file part): the core records the delimiter and
    /// header in the saved file's interpretation attribute, if the file is
    /// still the one saved (`rememberReviewedInterpretation`), so a reopen
    /// reads it as the document does. Inside a coordinated write of the
    /// file's metadata, with the document as presenter so it doesn't hear
    /// of its own write. Not while another save runs: its own reading is
    /// reviewed in turn.
    private func rememberReviewedInterpretation(model: DocumentModel) {
        guard self.model === model, !model.isSaving, let url = fileURL else { return }
        guard let handle = model.backgroundHandle() else { return }
        remembering = Task { await Self.remember(url, handle: handle, document: self) }
    }

    /// Called off the main thread while `remember` holds its coordinated
    /// write, before the core's call. Tests set it, to hold the write.
    nonisolated(unsafe) static var whileRememberingForTesting: (@Sendable () -> Void)?
    /// Called off the main thread once `remember` has ended its coordinated
    /// write. Tests set it.
    nonisolated(unsafe) static var afterRememberingForTesting: (@Sendable () -> Void)?

    /// `rememberReviewedInterpretation`, off the main thread from the
    /// moment it has its coordinated write until it ends it (task 2.5.3b
    /// review). Other apps' coordinated access to the file waits for the
    /// write, and the main thread may be blocked meanwhile: `NSDocument`
    /// blocks it wherever it reads its own `isDocumentEdited` while a save
    /// holds the document's file access (the 2.5.3a rule). So nothing here
    /// waits for the main actor while the write is held. It takes no file
    /// access of its own: asking for one while a save holds one blocks the
    /// main thread. (Coordinated writes with the same presenter don't wait
    /// for each other, so Leal's own save doesn't wait for this one; the
    /// core keeps the two apart.) Without a coordinated write (the
    /// coordinator refused, or the wait was cancelled), the core isn't
    /// called. Returns whether the attribute was written.
    nonisolated private static func remember(_ url: URL, handle: LealFFI.Document, document: CSVDocument) async -> Bool {
        var handle: LealFFI.Document? = handle
        // Off the main thread: it may hold the last reference to the
        // core's document (`CoreRelease`).
        defer { CoreRelease.later(&handle) }
        let write: CoordinatedWrite
        do {
            write = try await CoordinatedWrite.begin(url, options: .contentIndependentMetadataOnly, presenter: document)
        } catch {
            Logger.document.error("Couldn’t coordinate recording the delimiter after a save: \(String(describing: error), privacy: .public)")
            return false
        }
        defer {
            write.end()
            afterRememberingForTesting?()
        }
        let core = handle
        let result = await FileWork.run(qos: .utility) { () -> Result<Bool, any Error> in
            whileRememberingForTesting?()
            return Result { try core?.rememberReviewedInterpretation() ?? false }
        }
        switch result {
        case let .success(wrote):
            if wrote { Logger.document.info("Recorded the delimiter after the saved file’s review") }
            return wrote
        case let .failure(error):
            Logger.document.error("Couldn’t record the delimiter after a save: \(String(describing: error), privacy: .public)")
            return false
        }
    }

    /// Says why Save didn't write the file and offers what the user can
    /// do: returns the choice, after doing it if it leads elsewhere (Save
    /// As, Duplicate, Save As UTF-8). Says nothing for a cancel, or for a
    /// failed document, which says so itself.
    @discardableResult
    private func refuse(_ failure: SaveFailure, model: DocumentModel) async -> SaveChoice {
        guard !model.isFailed, let refusal = SaveText.saveFailure(failure, name: fileName, headerRows: model.headerRows) else {
            return .cancel
        }
        let choice = await ask(refusal)
        switch choice {
        case .duplicate, .saveAs, .saveAsUTF8:
            followUp(choice)
        default:
            break
        }
        return choice
    }

    /// Shows `refusal` as a sheet and waits for the choice; the way out
    /// (its last) if there is no window. Return is the first choice's,
    /// unless that one writes over another app's changes (Save Anyway):
    /// then Return is Cancel's.
    private func ask(_ refusal: SaveText.Refusal) async -> SaveChoice {
        let way = refusal.choices.last ?? .cancel
        guard let window = windowControllers.first?.window else { return way }
        let alert = NSAlert()
        alert.messageText = refusal.title
        alert.informativeText = refusal.detail
        if let details = refusal.details {
            // Small, selectable type under the text, so it can be copied
            // into a report.
            let field = NSTextField(wrappingLabelWithString: details)
            field.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
            field.textColor = .secondaryLabelColor
            field.isSelectable = true
            field.preferredMaxLayoutWidth = 260
            field.frame.size = field.fittingSize
            alert.accessoryView = field
        }
        let destructive = refusal.choices.contains(.saveAnyway)
        for choice in refusal.choices {
            let button = alert.addButton(withTitle: SaveText.button(choice))
            button.hasDestructiveAction = choice == .saveAnyway
            if choice == .cancel {
                button.keyEquivalent = destructive ? "\r" : "\u{1b}"
            } else if destructive, button.keyEquivalent == "\r" {
                button.keyEquivalent = ""
            }
        }
        return await withCheckedContinuation { continuation in
            showSheet(alert, window) { response in
                let index = response.rawValue - NSApplication.ModalResponse.alertFirstButtonReturn.rawValue
                continuation.resume(returning: refusal.choices.indices.contains(index) ? refusal.choices[index] : way)
            }
        }
    }

    /// Duplicate, Save As… and Save As UTF-8… from a Save alert, each
    /// through the core (`chooseSaveAs`).
    private func followUp(_ choice: SaveChoice) {
        if let saveFollowUpForTesting { return saveFollowUpForTesting(choice) }
        switch choice {
        case .duplicate:
            chooseSaveAs(.duplicate)
        case .saveAs:
            chooseSaveAs(.saveAs)
        case .saveAsUTF8:
            (windowControllers.first as? DocumentWindowController)?.content.saveAsUTF8(nil)
        default:
            break
        }
    }

    /// Whether anything is at `url` (a link that leads nowhere counts as
    /// nothing). It touches the file: call it off the main thread.
    nonisolated private static func exists(_ url: URL) -> Bool {
        var info = stat()
        return stat(url.path(percentEncoded: false), &info) == 0
    }

    /// The user locks (Finder's Locked, `uchg`, and append-only, `uappnd`),
    /// which Unlock clears, and the system's (`schg`, `sappnd`), which only
    /// an administrator can. The core's Locked is any of them.
    nonisolated private static let userLocks = UInt32(UF_IMMUTABLE | UF_APPEND)
    nonisolated private static let systemLocks = UInt32(SF_IMMUTABLE | SF_APPEND)

    /// Whether the system locked the file (`schg` or `sappnd`), which Leal
    /// can't undo: Save then offers no Unlock. It touches the file: call it
    /// off the main thread.
    nonisolated static func isLockedBySystem(_ url: URL) -> Bool {
        var info = stat()
        guard stat(url.path(percentEncoded: false), &info) == 0 else { return false }
        return info.st_flags & systemLocks != 0
    }

    /// Clears the file's user locks (immutable and append-only, as the
    /// Finder's Locked does), for Save's **Unlock**. Returns whether it is
    /// unlocked: not if the system's locks are on too (`schg`, `sappnd`),
    /// which only an administrator can clear. It touches the file: call it
    /// off the main thread.
    nonisolated static func unlock(_ url: URL) -> Bool {
        let path = url.path(percentEncoded: false)
        var info = stat()
        guard stat(path, &info) == 0, info.st_flags & systemLocks == 0 else { return false }
        guard info.st_flags & userLocks != 0 else { return true }
        return chflags(path, info.st_flags & ~userLocks) == 0
    }

    // MARK: Save As and Duplicate (task 2.5.3c)

    /// What the save panel is asked: the name and folder it suggests, and
    /// the message saying the copy will be incomplete, if it will be.
    struct SaveAsRequest: Equatable {
        let name: String
        let folder: URL
        let message: String?
    }

    /// Asks where to Save As: the save panel, as a sheet on `window` (on
    /// its own with none), which asks itself before replacing a file;
    /// `done` hears the place, or `nil` if cancelled. In the sandbox the
    /// panel gives Leal access to the place chosen. Tests answer for
    /// themselves, with no panel.
    var chooseSaveAsDestination: (_ request: SaveAsRequest, _ window: NSWindow?, _ done: @escaping @MainActor (URL?) -> Void) -> Void = { request, window, done in
        let panel = NSSavePanel()
        panel.nameFieldStringValue = request.name
        panel.directoryURL = request.folder
        panel.canCreateDirectories = true
        panel.isExtensionHidden = false
        if let message = request.message { panel.message = message }
        let finish: (NSApplication.ModalResponse) -> Void = { response in
            let url = response == .OK ? panel.url : nil
            MainActor.assumeIsolated { done(url) }
        }
        if let window {
            panel.beginSheetModal(for: window, completionHandler: finish)
        } else {
            panel.begin(completionHandler: finish)
        }
    }

    /// Puts a file Save As wrote in Open Recent. Tests replace it, so the
    /// test host's recent documents (Leal's own) stay as they are.
    static var noteRecentDocument: (URL) -> Void = { NSDocumentController.shared.noteNewRecentDocumentURL($0) }

    /// How Save As was asked for.
    enum SaveAsMode {
        /// File ▸ Save As…, and the Save As… of alerts and banners: the
        /// file's own name suggested.
        case saveAs
        /// Duplicate, from Save's alerts for a file Leal can't write (and
        /// AppKit's `duplicateDocument:`): "name copy" suggested.
        case duplicate
    }

    /// **Save As…** (File ▸ Save As…, ⇧⌘S): through the core, never
    /// `NSDocument`'s own panel and writing (`chooseSaveAs`).
    override func saveAs(_ sender: Any?) {
        chooseSaveAs(.saveAs)
    }

    /// **Duplicate** keeps the edits in a copy the user names, and the
    /// window then edits the copy, as after Save As: Save's alerts offer
    /// it for a file Leal can't write (locked, read-only), whose edits
    /// would otherwise have nowhere to go. A Leal document is always a
    /// file (its core reads one), so there is no untitled duplicate.
    override func duplicate(_ sender: Any?) {
        chooseSaveAs(.duplicate)
    }

    /// File ▸ **Duplicate** (⇧⌘D): `duplicate(_:)`, under an action of
    /// Leal's own. `NSDocument` hides any item whose action is
    /// `duplicateDocument:` when the app doesn't autosave in place, as
    /// Leal doesn't (`validateUserInterfaceItem`, seen on macOS 26), so
    /// the menu item can't use it.
    @objc func saveDuplicate(_ sender: Any?) {
        duplicate(sender)
    }

    /// AppKit's own Save As panel, should anything ask for it: Leal's Save
    /// As instead, the delegate told whether it saved. Nothing else
    /// (Export) is offered.
    override func runModalSavePanel(
        for saveOperation: NSDocument.SaveOperationType,
        delegate: Any?,
        didSave didSaveSelector: Selector?,
        contextInfo: UnsafeMutableRawPointer?
    ) {
        guard saveOperation == .saveAsOperation else {
            Logger.document.fault("NSDocument's save panel was asked for (operation \(saveOperation.rawValue))")
            return Self.answer(delegate, didSaveSelector, document: self, flag: false, contextInfo: contextInfo)
        }
        chooseSaveAs(.saveAs) { [weak self] saved in
            guard let self else { return }
            Self.answer(delegate, didSaveSelector, document: self, flag: saved, contextInfo: contextInfo)
        }
    }

    /// Where every `NSDocument` save ends: Leal never lets it write
    /// (ADR-0012). A Save As to `url` goes through the core; anything
    /// else is refused.
    override func save(
        to url: URL,
        ofType typeName: String,
        for saveOperation: NSDocument.SaveOperationType,
        completionHandler: @escaping ((any Error)?) -> Void
    ) {
        guard saveOperation == .saveAsOperation, model != nil else {
            Logger.document.fault("NSDocument tried to write the file itself (operation \(saveOperation.rawValue))")
            return completionHandler(NSError(domain: NSCocoaErrorDomain, code: NSFeatureUnsupportedError))
        }
        let task = saveAs(to: url)
        Task {
            let saved = await task.value
            completionHandler(saved ? nil : NSError(domain: NSCocoaErrorDomain, code: NSUserCancelledError))
        }
    }

    /// Asks where to save (`chooseSaveAsDestination`), then saves there
    /// (`saveAs(to:)`). An open edit is committed first, so it is in the
    /// copy (or, if the core refuses it, it stays open and nothing is
    /// saved). If Leal hasn't read all of the file, the panel says plainly
    /// that the copy will be incomplete ("about N of M rows", ADR-0008
    /// decision 6). `completion` hears whether it saved.
    func chooseSaveAs(_ mode: SaveAsMode, then completion: (@MainActor (Bool) -> Void)? = nil) {
        guard let model, !model.isFailed, !isRereading, let current = fileURL, commitEditing() else {
            NSSound.beep()
            completion?(false)
            return
        }
        let request = SaveAsRequest(
            name: mode == .duplicate ? SaveText.copyName(of: current) : current.lastPathComponent,
            folder: current.deletingLastPathComponent(),
            message: model.incompleteCopy.map { SaveText.incompleteCopyMessage(rows: $0.rows, of: $0.of) }
        )
        chooseSaveAsDestination(request, windowControllers.first?.window) { [weak self] url in
            guard let self, let url else {
                completion?(false)
                return
            }
            let task = saveAs(to: url)
            if let completion {
                Task { completion(await task.value) }
            }
        }
    }

    /// **Save As** to `url` (ADR-0012, ADR-0008 decision 1): the core
    /// writes the document there, with its edits, inside a coordinated
    /// write of `url`, and the document is then that file: the window
    /// shows the core's reading of it, and `NSDocument` (title, proxy icon,
    /// Open Recent), the model and the watcher follow it. The file the
    /// document had is untouched. Queued behind any Save under way.
    ///
    /// Over a file that is there already it replaces it, keeping that
    /// file's metadata, as a Save does (the panel asked first). To the
    /// document's own file it is a Save, with Save's checks.
    ///
    /// If Leal couldn't read all of the file (ADR-0008 decision 6, ADR-0010:
    /// a drive or share gone, the file changed or deleted while read),
    /// the copy has only the complete rows Leal trusts, and an alert says
    /// so, naming the edits it couldn't keep. Why it couldn't save is
    /// said too, offering Save As UTF-8 where that would. Returns its task,
    /// whose value says whether it saved.
    ///
    /// Refused (`false`, nothing written) while a Reload or Revert reads
    /// the file again (task 2.5.3c review).
    @discardableResult
    func saveAs(to url: URL) -> Task<Bool, Never> {
        guard !isRereading else {
            NSSound.beep()
            return Task { false }
        }
        return enqueueSave { document in
            // The document's own file, however `url` spells it (case,
            // Unicode normalization, a link): a Save. Decided by identity,
            // off the main thread; the core decides the same way again
            // as it saves.
            if let current = document.fileURL, await FileWork.run({ Self.isSameFile(url, current) }) {
                return await document.saveInPlace()
            }
            return await document.saveAsAndSay(url)
        }
    }

    /// Whether `a` and `b` name the same file (its volume and inode,
    /// symbolic links followed). Any thread: it looks at the files.
    nonisolated static func isSameFile(_ a: URL, _ b: URL) -> Bool {
        if a.standardizedFileURL == b.standardizedFileURL { return true }
        var first = stat()
        var second = stat()
        guard stat(a.path(percentEncoded: false), &first) == 0, stat(b.path(percentEncoded: false), &second) == 0 else {
            return false
        }
        return first.st_dev == second.st_dev && first.st_ino == second.st_ino
    }

    /// One Save As or Save As UTF-8, queued behind any Save under way; the
    /// waiting task's cancel cancels it.
    private func saveAsQueued(_ url: URL, kind: SaveKind) async -> Result<DocumentModel.Saved?, SaveFailure> {
        let box = SaveAsResult()
        let task = enqueueSave { document in
            box.result = await document.saveAsNow(url, kind: kind)
            return (try? box.result.get()) != nil
        }
        _ = await withTaskCancellationHandler {
            await task.value
        } onCancel: {
            task.cancel()
        }
        return box.result
    }

    /// What a queued Save As ended with.
    @MainActor
    private final class SaveAsResult {
        var result: Result<DocumentModel.Saved?, SaveFailure> = .success(nil)
    }

    /// Save As to `url`, its turn come, then what the user is told: an
    /// incomplete copy, or why it wasn't saved. Whether it saved.
    private func saveAsAndSay(_ url: URL) async -> Bool {
        guard let model else { return false }
        switch await saveAsNow(url, kind: .saveAs) {
        case let .success(saved?):
            lastSaveFailure = nil
            lastSaveOutcome = saved.outcome
            return true
        case .success(nil):
            return false
        case let .failure(failure):
            lastSaveFailure = failure
            Self.log(failure, url: url, what: "Save As")
            guard !model.isFailed,
                  let refusal = SaveText.saveAsFailure(failure, name: url.lastPathComponent, headerRows: model.headerRows)
            else { return false }
            if await ask(refusal) == .saveAsUTF8 { followUp(.saveAsUTF8) }
            return false
        }
    }

    /// The Save As itself (`kind` `.saveAs` or `.saveAsUtf8`), its turn
    /// come: as Save's `saveCoordinated`, then `saveFinished`, which makes
    /// the document the new file; a kept old file (Save As over a file) is
    /// reported, and so is an incomplete copy (`reportIncomplete`).
    /// `nil` if the document failed or closed.
    private func saveAsNow(_ url: URL, kind: SaveKind) async -> Result<DocumentModel.Saved?, SaveFailure> {
        guard let model, !model.isFailed else { return .success(nil) }
        let result = await saveCoordinated(url, kind: kind, overwriteChanged: false, model: model)
        if case let .success(saved?) = result {
            if let reporting = saveFinished(saved, kind: kind, model: model) {
                _ = await reporting.value
            }
            await reportIncomplete(saved.outcome, model: model)
        }
        return result
    }

    /// The copy a Save As wrote is incomplete (ADR-0008 decision 6): say
    /// so plainly, "about N of M rows", naming the edits it didn't keep.
    /// The counts leave out the header row, as the status bar does.
    private func reportIncomplete(_ outcome: SaveOutcome, model: DocumentModel) async {
        guard !outcome.complete else { return }
        let header = model.headerRows
        let rows = max(0, Int(outcome.rowCount) - header)
        let total = max(rows, Int(outcome.estimatedRowCount) - header)
        let name = URL(filePath: outcome.path).lastPathComponent
        _ = await ask(SaveText.incompleteCopy(name: name, rows: rows, of: total, skipped: outcome.skippedEdits, headerRows: header))
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
        if saving != nil {
            // Whatever it does, a command during a save keeps the document
            // edited until the save ends: the save's token, noted before
            // it, mustn't mark it saved, and closing and quitting wait for
            // the save before they decide (`savesEnded`).
            updateChangeCount(.changeDone)
        } else {
            updateDirtyState()
        }
        history.noteToken(changeCountToken(for: .saveOperation), version: version)
    }

    /// An undo or redo step: the core applies it, and it comes back
    /// through `commandApplied`. A refusal waits for the undo manager to
    /// finish (`stepEnded`).
    private func applyStep(_ command: EditCommand, _ direction: CommandDirection) {
        guard let model else { return }
        let outcome = direction == .undo ? model.undo(command) : model.redo(command)
        if case let .refused(refusal) = outcome {
            // The step's own name ("Duplicate Row", "Delete Columns"), not
            // its command's.
            let name = history.undoManager.stepName ?? EditHistory.actionName(for: command)
            refusedStep = (direction, refusal, name)
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
        guard let (direction, refusal, name) = refusedStep else { return }
        refusedStep = nil
        let temporary = HistoryText.isTemporary(refusal)
        if !temporary {
            history.undoManager.removeAllActions()
        }
        NSSound.beep()
        guard let window = windowControllers.first?.window else { return }
        let alert = NSAlert()
        alert.messageText = HistoryText.undoRefused(name, undo: direction == .undo)
        alert.informativeText = temporary ? HistoryText.undoNotYetDetail(refusal) : HistoryText.undoRefusedDetail(refusal)
        showSheet(alert, window) { _ in }
    }

    /// The document is edited while some cell reads differently from the
    /// file (the core's `hasUnsavedEdits`, DESIGN §3.6), so an edit set
    /// back by hand, or undone, leaves it clean. The change count is
    /// `NSDocument`'s, moved by this alone, so a save's token (2.5.3) marks
    /// the edits up to its snapshot as saved and those after as not.
    /// While a save runs it is never cleared: closing or quitting then
    /// would lose an edit set back by hand during the save, which the save
    /// doesn't write (task 2.5.3a review). It is cleared once every save
    /// has ended (`savesEnded`).
    func updateDirtyState() {
        guard let model, !model.isFailed else { return }
        if model.hasUnsavedEdits {
            updateChangeCount(.changeDone)
        } else if isDocumentEdited, saving == nil {
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

    /// Asks before a Reload or Revert to Saved throws the unsaved edits
    /// away (ADR-0008 decision 4). `answer` hears whether to go on.
    func confirmDiscardingEdits(_ reason: RereadReason, _ answer: @escaping @MainActor (Bool) -> Void) {
        guard let window = windowControllers.first?.window else { return answer(true) }
        let alert = NSAlert()
        let name = displayName ?? ""
        alert.messageText = reason == .revert ? HistoryText.discardForRevert(name) : HistoryText.discardForReload(name)
        alert.informativeText = HistoryText.discardForReloadDetail
        let reload = alert.addButton(withTitle: reason == .revert ? HistoryText.revert : HistoryText.reload)
        reload.hasDestructiveAction = true
        alert.addButton(withTitle: HistoryText.cancel)
        showSheet(alert, window) { response in answer(response == .alertFirstButtonReturn) }
    }

    /// Closing commits an open edit first, so it counts as an unsaved
    /// change, then asks as `NSDocument` does (Save, Don't Save, Cancel).
    /// Save then saves through the core (`save(withDelegate:didSave:contextInfo:)`).
    /// During a Save it waits for the save to end, without blocking the
    /// main thread, and then decides: what the save wrote, and an edit
    /// made meanwhile, are known only then.
    override func canClose(withDelegate delegate: Any, shouldClose shouldCloseSelector: Selector?, contextInfo: UnsafeMutableRawPointer?) {
        guard commitEditing() else {
            // The core refused the open edit: it stays open, saying why.
            return Self.answer(delegate, shouldCloseSelector, document: self, flag: false, contextInfo: contextInfo)
        }
        if saving != nil {
            let decide: @MainActor () -> Void = { [weak self] in
                self?.canClose(withDelegate: delegate, shouldClose: shouldCloseSelector, contextInfo: contextInfo)
            }
            Task { [weak self] in
                await self?.waitForSaves()
                decide()
            }
            return
        }
        if isDocumentEdited, let prompt = unsavedChangesPromptForTesting {
            prompt { [weak self] close in
                guard let self else { return }
                Self.answer(delegate, shouldCloseSelector, document: self, flag: close, contextInfo: contextInfo)
            }
            return
        }
        super.canClose(withDelegate: delegate, shouldClose: shouldCloseSelector, contextInfo: contextInfo)
    }

    /// Calls `canClose`'s or Save's delegate, as `NSDocument` does:
    /// `document:shouldClose:contextInfo:` or `document:didSave:contextInfo:`.
    private static func answer(_ delegate: Any?, _ selector: Selector?, document: NSDocument, flag: Bool, contextInfo: UnsafeMutableRawPointer?) {
        guard let selector, let object = delegate as? NSObject, object.responds(to: selector) else { return }
        typealias Callback = @convention(c) (NSObject, Selector, NSDocument, Bool, UnsafeMutableRawPointer?) -> Void
        let callback = unsafeBitCast(object.method(for: selector), to: Callback.self)
        callback(object, selector, document, flag, contextInfo)
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
    ///
    /// A save resets the journal to the commands after its snapshot
    /// (`saveFinished`), so Recover changes never replays saved edits on top
    /// of the saved file.
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
            if response == .alertFirstButtonReturn { self?.chooseSaveAs(.saveAs) }
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
