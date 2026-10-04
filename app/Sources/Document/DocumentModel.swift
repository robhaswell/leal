import AppKit
import LealFFI
import os

/// What the window needs to open documents: the app's one scheduler (DESIGN
/// §3.10) and where temporary files go. Tests pass their own.
struct DocumentEnvironment: Sendable {
    let scheduler: Scheduler
    let temp: TempLocations
}

/// What changed in a `DocumentModel`, for the window.
enum DocumentChange: Equatable, Sendable {
    /// Row or column counts, indexing progress or the line ending.
    case progress
    /// Column widths or alignment.
    case columns
    /// The file was read again with other choices (the Header row toggle):
    /// every value, title and count may be different.
    case content
    /// Rows already shown may read differently now (the file changed while
    /// it was read, or it is read again after its drive came back): redraw
    /// every cell, keeping the selection and scroll position.
    case rows
    /// **Reload** (task 1.9) opened the file again: every value, title and
    /// count may be different. The window keeps its scroll position and
    /// selection where they still fit.
    case reloaded
    /// The document failed after a panic in the core (DESIGN §3.9).
    case failed
    /// The values of these grid rows changed (an edit, an undo or a redo:
    /// `cellsChanged(rows:)`): redraw them.
    case cells(Range<Int>)
    /// The header row's titles changed (an edit to file row 0 while it is
    /// the header row, task 2.5.1): redraw the column headers.
    case header
    /// Only the column widths changed: an edited value is wider than its
    /// column (task 2.5.1).
    case widths
    /// Rows or a column were inserted or deleted (an undo or a redo of
    /// one, task 2.5.2): every row after the change moved, and the column
    /// count and widths may be different.
    case structure
    /// A save rebased the document onto the file it wrote (task 2.5.3b):
    /// a new reading of the same values. Every cell is drawn again (the
    /// edited-cell marks of saved edits go), and the selection, scroll
    /// position, column widths and an open editor stay.
    case saved
}

/// How the reading under the edits changed (task 2.5.2), for the edit
/// history.
enum ReadingChange: Equatable, Sendable {
    /// Read again with the same split (the Header row toggle): the edits
    /// and their undo history stay.
    case sameSplit
    /// Read with another delimiter or encoding (Treat As, Reopen with
    /// Encoding), which only a document with no edits can be: a new
    /// lineage, so the commands before it no longer apply.
    case newSplit
    /// Reloaded (Reload, Revert, a Save As UTF-8's copy): a new core
    /// document, with no edits.
    case replaced
}

/// One open file, as the window shows it: the core's `Document` (through
/// FFI), presented to the grid as a `GridDataSource`.
///
/// It reads nothing about the file's contents itself: rows, counts, the
/// dialect, the encoding and which columns are numeric all come from the
/// core. It owns the Swift tasks that wait for the core's background jobs
/// (each through `Job.finish()`, so cancelling the task stops the job),
/// coalesces progress for the main thread, and measures column widths.
///
/// **After a panic** in the core the document has failed (DESIGN §3.9):
/// the model makes no further calls on its handle, shows no rows, and
/// reports `.failed`; the window offers to reopen the file.
@MainActor
final class DocumentModel: GridDataSource {
    /// Rows the first sizing and number detection read, at P0 (a tall
    /// screen's worth).
    nonisolated static let firstScreenRows: UInt32 = 100
    /// Rows the refined column sizing and number detection read (ADR-0002
    /// question 2; P2 work in DESIGN §3.10).
    nonisolated static let sizingRows: UInt32 = 1_000
    /// The most fields (rows × columns) a sizing read takes, as the core's
    /// row cache caps fields: a file of 20,000 columns is sized from 5 rows,
    /// not 1,000 (20M cells).
    nonisolated static let sizingFieldLimit = 100_000
    /// The most fields one core call of the refined sizing reads: about a
    /// screenful, so the reading's row cache is never held for long by a
    /// utility thread while the grid waits for it (phase 1 review, app-10).
    nonisolated static let sizingBatchFields = 5_000

    /// Called after first paint inside `init`, on the core's document. Only
    /// tests set it, to make the document fail while it opens.
    static var afterFirstPaintForTesting: ((LealFFI.Document) throws -> Void)?

    /// Makes `setCell` refuse as the core would, with nothing changed. Only
    /// tests set it: the core refuses an edit the editor opened on only
    /// when the file changes under it, which closes the editor.
    var refusalForTesting: EditRefusal?
    /// Opens the core's document in place of `openDocument`. Only tests set
    /// it, to open a file as if on a removable drive that vanishes, or
    /// whose file changes, part-way through (`debugOpenDocumentWithFault`,
    /// task 1.7), or on a network share (`debugOpenDocumentSimulatingShare`,
    /// task 2.0). It is read off the main thread (`openCore`), so it is
    /// kept behind a lock.
    nonisolated static var openForTesting: OpenHook? {
        get { openForTestingLock.withLock { $0 } }
        set { openForTestingLock.withLock { $0 = newValue } }
    }
    typealias OpenHook = @Sendable (_ path: String, _ environment: DocumentEnvironment, _ options: OpenOptions, _ observer: any ProgressObserver) throws -> LealFFI.Document
    nonisolated private static let openForTestingLock = OSAllocatedUnfairLock<OpenHook?>(initialState: nil)
    /// How often a disconnected file on a network share is looked at again
    /// (task 2.0 review): an SMB session that reconnects by itself fires no
    /// mount notification. Tests shorten it.
    static var shareRecheckInterval: Duration = .seconds(5)
    /// Where the model hears of mounts and app activations: the system's
    /// centres, or a test's own, so that another process's disk image or a
    /// window of another app coming and going doesn't reach the test.
    static var notificationCentersForTesting: (workspace: NotificationCenter, app: NotificationCenter)?
    /// Rows per block of the row-flags cache (gutter markers and hatching).
    nonisolated static let flagBlockRows = 64

    /// Where the file is: where it was opened from, or where it was moved
    /// to since (task 1.9).
    private(set) var url: URL
    private var handle: LealFFI.Document? {
        didSet { readAheadHandle.set(failure == nil ? handle : nil) }
    }
    /// The core document for the grid's reads ahead (task 2.0a), which
    /// they look up when they run, not when they are queued: once the
    /// document closes, fails or is replaced, a queued read reads nothing
    /// and holds nothing.
    private let readAheadHandle = ReadAheadHandle<LealFFI.Document>()
    /// Goes up by one each time **Reload** replaces `handle`, so a late
    /// report about the old core document is ignored.
    private var handleNumber = 0

    /// Which core document and which reading of it the model shows. A
    /// background answer is used only if this hasn't changed meanwhile: a
    /// generation alone isn't enough, since a reloaded document starts at
    /// generation 0 again (task 1.9).
    var readingID: ReadingID { ReadingID(handle: handleNumber, generation: generation) }
    private let environment: DocumentEnvironment
    private var scheduler: Scheduler { environment.scheduler }

    /// Called on the main actor when something the window shows changed.
    var onChange: ((DocumentChange) -> Void)?
    /// A command was applied (an edit, or an undo or redo; task 2.5.1),
    /// and which way. SEAM(2.5.2): undo and the recovery journal register
    /// it here (`commandApplied`).
    var onCommand: ((EditCommand, CommandDirection) -> Void)?
    /// The file was read again, or replaced, under the edits (task 2.5.2):
    /// the edit history follows.
    var onReadingChanged: ((ReadingChange) -> Void)?
    /// Whether some cell reads differently from the file (the core's
    /// `hasUnsavedEdits`), as of the last command or reading. It stays as
    /// it was once the document fails: its edits are recovered from the
    /// journal.
    private(set) var hasUnsavedEdits = false
    /// Recover changes is under way (`recover`).
    private(set) var isRecovering = false
    /// Called when the file was moved, with its new place (task 1.9), so
    /// the `NSDocument` follows it.
    var onMoved: ((URL) -> Void)?

    private(set) var interpretation: Interpretation
    private(set) var generation: UInt64
    private(set) var progress: IndexProgress
    /// The whole file's line ending, once the review (P2) has finished.
    private(set) var reviewedLineEnding: LineEnding?
    /// The most common field count: the "× N columns" of the status bar.
    private(set) var fileColumnCount: Int
    /// Where the file's bytes are held (DESIGN §3.1): a clone, in memory or
    /// a copy when the volume can't clone, and for a removable drive, being
    /// read, copied, or disconnected (ADR-0006). The status bar notes all
    /// but the clone.
    private(set) var storage: SourceStorage = .clone
    /// The file changed on its drive while it was read (1.1a): the rows
    /// shown may mix two versions.
    private(set) var changedOnDisk = false
    /// The file is on a network share (ADR-0009). While it is disconnected,
    /// the model looks at it again every `shareRecheckInterval`.
    private(set) var isOnNetworkShare = false
    /// A Reload is under way (`reloadInBackground`): Reload, Treat As,
    /// Reopen with Encoding and the Header row are off until it is done.
    private(set) var isReloading = false
    /// The edit version when the Reload under way was asked for
    /// (`willReload`): one made since stops it being adopted.
    private var reloadVersion: UInt64?
    /// Looks at a disconnected share's file again, now and then.
    private var shareRecheck: Task<Void, Never>?
    /// Where the index had got to at the share's last disconnection, and
    /// how many disconnections in a row stopped there: after
    /// `shareRecheckLimit`, the periodic check stops, so a bad block doesn't
    /// reconnect and fail for ever (task 2.0 re-review). A mount, an app
    /// activation or a Reload starts it again.
    private var lastDisconnection: (generation: UInt64, offset: UInt64, times: Int)?
    /// See `lastDisconnection`.
    nonisolated static let shareRecheckLimit = 3
    /// The index stopped before the end of the file, on a read error (a
    /// drive still there but failing, or the internal disk full while
    /// copying): no more rows will come for this reading, so the window
    /// shows the rows read, says so, and offers Reload (phase 1 review,
    /// app-8).
    private(set) var readStopped = false
    /// The index stopped before the end of the file because the drive was
    /// disconnected or the file changed while it was read: no more rows
    /// come for this reading (the drive coming back starts a new one), so
    /// the window shows the rows read and no progress. Their banners say
    /// why (phase 1 review).
    private(set) var indexStopped = false
    /// Whether Save could write over the file: not after a disconnection
    /// or a change while reading, nor while its drive is away (ADR-0006).
    /// Save As always can.
    private(set) var canSave = true
    /// The user's file as last seen (task 1.9): changed, moved or deleted
    /// elsewhere, or on a drive that isn't connected.
    private(set) var original: OriginalStatus
    /// A `checkOriginal` is under way.
    private var checkingOriginal: Task<Void, Never>?
    /// Another check was asked for meanwhile (a volume unmounting, then
    /// mounting): it runs when this one ends.
    private var checkAgain = false
    /// Notifications that make the model look at the file again.
    private var observers: [(NotificationCenter, any NSObjectProtocol)] = []
    /// What the index has found wrong with the file so far (DESIGN §3.5),
    /// for the current reading.
    private(set) var diagnostics: DiagnosticsReport?
    /// What the review (P2) suggests, once it has finished.
    private(set) var review: ReviewResult?
    /// The reading shown is the one a save made of the file it wrote (task
    /// 2.5.3b), not one read again since.
    private(set) var readingFromSave = false
    /// After a save, the index says it is complete at once (its index and
    /// field counts come from the save's plan), but its pass still builds
    /// the diagnostics (task 2.4c): only a complete report is taken.
    private var awaitingCompleteDiagnostics = false
    /// The review of a save's reading has finished (task 2.5.3b): it may
    /// add the interpretation attribute to the saved file
    /// (`CSVDocument.rememberReviewedInterpretation`).
    var onSavedReadingReviewed: (() -> Void)?
    /// Row flags read from the core, by block of `flagBlockRows` grid rows.
    private var flagBlocks: [Int: [RowFlags]] = [:]
    /// A drive-state check is queued (see `call`).
    private var driveCheckQueued = false
    /// Why the document failed, once it has.
    private(set) var failure: (any Error)?
    /// How many calls the model has made on the core's document, so a test
    /// can show none are made after a failure.
    private(set) var coreCalls = 0

    private(set) var columnCount = 0
    private(set) var columnWidths: [CGFloat] = []
    private var numeric: [Bool] = []
    private var headerTitles: [String] = []
    /// Columns the user has resized; sizing leaves them alone.
    private var resizedColumns: Set<Int> = []
    /// Each column's widest edited value outside the sample rows (task
    /// 2.5.1), so that measuring the sample again after an edit doesn't
    /// narrow a column such an edit widened.
    private var editedWidest: [Int: CGFloat] = [:]
    /// Widths and number detection measured again after an edit inside the
    /// sample rows, as after a reinterpret (task 2.5.1).
    private var sizingAfterEdit: Task<Void, Never>?
    /// The refined sizing under way is a measuring again after an edit.
    private var remeasuring = false
    /// Each column's widest text in the rows the widths were measured from
    /// (up to `GridMetrics.fitMaximumWidth`), kept for double-click to fit
    /// instead of the rows themselves.
    private var widestText: [CGFloat] = []
    /// The most fields a row read for sizing had.
    private var widestSampleRow = 0
    /// The most fields one sizing read has taken, for tests.
    private(set) var largestSizingRead = 0
    /// This document's scroll gesture is under way (see `setInteracting`).
    private var interacting = false
    private var tiles: CellTileCache!
    private var tasks: [Task<Void, Never>] = []
    private var refinedSizingStarted = false
    /// The refined sizing of this reading has arrived (its last change to
    /// the columns), for tests that wait for the model to settle.
    private(set) var isSizingRefined = false
    private var columnUpdateScheduled = false

    private let cellMeasurer = TextMeasurer(font: GridFonts.cell)
    private let numberMeasurer = TextMeasurer(font: GridFonts.number)
    private let headerMeasurer = TextMeasurer(font: GridFonts.header)

    /// Opens `url` with the core: first paint (P0) happens before this
    /// returns, and the index (P1) and review (P2) start in the background.
    /// It reads the file on this thread; for a file that may be on a network
    /// share, open it off the main thread with `openCore` and make the model
    /// with `make(from:)` (ADR-0009, `CSVDocument.read(from:ofType:)`).
    static func open(url: URL, environment: DocumentEnvironment) throws -> DocumentModel {
        try make(from: openCore(url: url, environment: environment))
    }

    /// The core's document for `url`, opened (first paint included) but
    /// without its model yet: `openCore` makes it on any thread, and
    /// `make(from:)` turns it into a model on the main actor. It hands its
    /// document over exactly once (`make(from:)` or `release()`), so
    /// whoever lets go of it last does so on purpose: never by a stray copy
    /// going out of scope on the main thread (task 2.0 review).
    final class OpenedCore: Sendable {
        let url: URL
        let environment: DocumentEnvironment
        let reference: ModelReference
        private let handle: OSAllocatedUnfairLock<LealFFI.Document?>

        nonisolated init(url: URL, environment: DocumentEnvironment, handle: LealFFI.Document, reference: ModelReference) {
            self.url = url
            self.environment = environment
            self.reference = reference
            self.handle = OSAllocatedUnfairLock(initialState: handle)
        }

        /// The core's document, which the caller now owns; `nil` once taken.
        nonisolated func takeHandle() -> LealFFI.Document? {
            handle.withLock { held in
                defer { held = nil }
                return held
            }
        }

        /// Lets go of the document, if it wasn't taken, off the main thread
        /// (`CoreRelease`).
        nonisolated func release() {
            var unused = takeHandle()
            CoreRelease.later(&unused)
        }
    }

    /// Opens `url` in the core: its first paint, which reads the file. Safe
    /// on any thread, and for a file on a network share it must be off the
    /// main thread, where a share that stops answering would freeze the app
    /// (ADR-0009; the core's debug builds check).
    nonisolated static func openCore(url: URL, environment: DocumentEnvironment) throws -> OpenedCore {
        let reference = ModelReference()
        // The first screen's rows are read later, through `cells`, with a
        // cap on the fields; the core's first screen need only bring the
        // first row, for the header titles.
        let options = OpenOptions(firstScreenRows: 1, maxChars: GridMetrics.maxCellCharacters)
        let handle = try openHandle(url: url, environment: environment, options: options, reference: reference, number: 0)
        return OpenedCore(url: url, environment: environment, handle: handle, reference: reference)
    }

    /// The model for a core document `openCore` opened. It reads only
    /// what first paint already holds (the first 64 KB), never the file.
    static func make(from core: OpenedCore) throws -> DocumentModel {
        guard let handle = core.takeHandle() else {
            throw LealError.Internal(message: "the opened document was already taken")
        }
        let model = try DocumentModel(url: core.url, handle: handle, environment: core.environment)
        core.reference.model = model
        return model
    }

    /// Opens `url` in the core, with progress for handle `number` relayed
    /// to the model `reference` will point at. Any thread: it reads the
    /// file and asks Foundation about its volume.
    nonisolated private static func openHandle(
        url: URL,
        environment: DocumentEnvironment,
        options: OpenOptions,
        reference: ModelReference,
        number: Int
    ) throws -> LealFFI.Document {
        let relay = ProgressRelay { progress in reference.model?.progressArrived(progress, handle: number) }
        let path = url.path(percentEncoded: false)
        return try openForTesting?(path, environment, options, relay) ?? openDocument(
            path: path,
            volume: TemporaryFolders.volume(for: url),
            temp: environment.temp,
            scheduler: environment.scheduler,
            options: options,
            observer: relay
        )
    }

    private init(url: URL, handle: LealFFI.Document, environment: DocumentEnvironment) throws {
        self.url = url
        self.handle = handle
        readAheadHandle.set(handle)
        self.environment = environment
        original = OriginalStatus(state: .unchanged, path: url.path(percentEncoded: false), diverged: false)
        let screen = try handle.firstScreen()
        interpretation = screen.interpretation
        generation = screen.generation
        fileColumnCount = Int(screen.columnCount)
        // First paint's counts: the rows in the first 64 KB and the estimate.
        // The index may already have more; the first report brings them.
        progress = IndexProgress(
            generation: screen.generation,
            rows: screen.rowCount,
            estimatedRows: screen.estimatedRowCount,
            bytesScanned: 0,
            bytesTotal: 0,
            complete: false
        )
        tiles = CellTileCache { [weak self] rows, columns in self?.readTile(rows: rows, columns: columns) }
        tiles.makeBackgroundFetch = { [weak self] in self?.backgroundTileReader() }
        tiles.onReadAheadError = { [weak self] error in self?.report(error) }
        applyFirstScreen(screen)
        original = call({ try $0.original() }) ?? original
        isOnNetworkShare = call({ try $0.isOnNetworkShare() }) ?? false
        refreshDriveState()
        if let hook = Self.afterFirstPaintForTesting {
            _ = call { try hook($0) }
        }
    }

    /// Starts waiting for the background jobs. Call once the window exists.
    func start() {
        guard failure == nil, tasks.isEmpty else { return }
        startWaiting()
        watchOriginal()
        observeVolumesAndActivation()
        progressArrived(progress)
    }

    /// Stops everything: the Swift tasks are cancelled, which cancels the
    /// jobs they wait for, and the core's document is released, which
    /// cancels the rest and deletes the file's clone (DESIGN §3.1). The
    /// release happens off the main thread (`CoreRelease`, app-10).
    func close() {
        setInteracting(false)
        for task in tasks { task.cancel() }
        tasks.removeAll()
        shareRecheck?.cancel()
        shareRecheck = nil
        tiles.removeAll()
        sizingAfterEdit?.cancel()
        stopObserving()
        CoreRelease.later(&handle)
        onChange = nil
        onMoved = nil
        onCommand = nil
        onReadingChanged = nil
    }

    var isFailed: Bool { failure != nil }

    /// Save is off: the file is UTF-16 (ADR-0013 decision 1). It can be
    /// edited, and Save As UTF-8 is the only way to save it.
    var isReadOnly: Bool {
        interpretation.encoding == .utf16Le || interpretation.encoding == .utf16Be
    }

    /// The first row is a header row, so the grid's row `r` is the file's
    /// row `r + 1`.
    private var headerOffset: Int { interpretation.header ? 1 : 0 }

    /// A scroll gesture in this document's grid began or ended (DESIGN
    /// §3.10 rule 3). The scheduler has no timeout, so a gesture this
    /// document started is always ended: by the grid, or when the document
    /// closes or fails. The scheduler is the app's, not the document's.
    func setInteracting(_ active: Bool) {
        guard active != interacting else { return }
        interacting = active
        scheduler.setInteracting(interacting: active)
    }

    // MARK: Calls on the core

    /// Runs `body` on the core's document, unless the document has failed.
    /// A `DocumentFailed` error, or anything that isn't a `LealError` (a
    /// panic UniFFI caught), fails the document; other errors (a removable
    /// drive that vanished, task 1.7's banner) are logged and give `nil`.
    func call<T>(_ body: (LealFFI.Document) throws -> T) -> T? {
        guard failure == nil, let handle else { return nil }
        coreCalls += 1
        do {
            return try body(handle)
        } catch {
            report(error)
            return nil
        }
    }

    /// What a core call's error means: a `DocumentFailed` error, or anything
    /// that isn't a `LealError` (a panic UniFFI caught), fails the
    /// document; a vanished drive or a changed or deleted file is shown;
    /// anything else is logged. For `call`, and for calls made off the main
    /// actor that hand their errors back (the grid's reads ahead).
    func report(_ error: any Error) {
        switch error as? LealError {
        case .DocumentFailed?, nil:
            fail(error)
        case .DriveDisconnected?, .ChangedOnDisk?, .DeletedElsewhere?:
            // A read found the drive gone, the file changed, or the file on
            // its share deleted: show it (task 1.7, ADR-0009). Not now,
            // though: this may be inside a draw.
            queueDriveCheck()
        case .some:
            Logger.document.error("Core call failed for \(self.url.lastPathComponent, privacy: .public): \(String(describing: error), privacy: .public)")
        }
    }

    /// The core's document, for a call off the main actor (task 1.8's cell
    /// inspector, like `find(_:forward:from:)` here): `nil` once the
    /// document has failed or closed. It counts as a core call; hand any
    /// error it throws back through `call`.
    func backgroundHandle() -> LealFFI.Document? {
        guard failure == nil, let handle else { return nil }
        coreCalls += 1
        return handle
    }

    /// DESIGN §3.9: no more calls on the handle, no rows, and the window
    /// offers to reopen the file.
    func fail(_ error: any Error) {
        guard failure == nil else { return }
        Logger.document.error("Document failed: \(String(describing: error), privacy: .public)")
        failure = error
        readAheadHandle.set(nil)
        setInteracting(false)
        for task in tasks { task.cancel() }
        tasks.removeAll()
        tiles.removeAll()
        sizingAfterEdit?.cancel()
        stopObserving()
        onChange?(.failed)
    }

    // MARK: Background jobs

    private func startWaiting() {
        guard let indexJob = call({ try $0.indexJob() }), let reviewJob = call({ try $0.reviewJob() }) else { return }
        let generation = generation
        let number = handleNumber
        let reading = readingID
        tasks.append(Task { [weak self] in
            do {
                try await indexJob.finish()
                self?.indexFinished(generation: generation, handle: number)
            } catch {
                self?.jobEnded(error, job: .index, reading: reading)
            }
        })
        tasks.append(Task { [weak self] in
            do {
                try await reviewJob.finish()
                self?.reviewFinished(generation: generation, handle: number)
            } catch {
                self?.jobEnded(error, job: .review, reading: reading)
            }
        })
    }

    /// The background jobs of a reading.
    enum BackgroundJob {
        /// The row index (P1).
        case index
        /// The whole-file review (P2).
        case review
    }

    /// Background job `job` of reading `reading` ended with `error` instead
    /// of finishing. What it says about rows is ignored once the file has
    /// been read again (a Reload, Treat As, a drive coming back), as for a
    /// job that finishes. Internal for tests, which end one with a panic or
    /// a read error.
    func jobEnded(_ error: any Error, job: BackgroundJob, reading: ReadingID) {
        let current = reading == readingID
        switch error as? JobFailure {
        case .Panicked?:
            fail(error)
        case .Cancelled?, nil:
            // Closing, re-reading or a cancelled task.
            break
        case .DriveDisconnected?, .ChangedOnDisk?, .DeletedElsewhere?:
            // The drive-disconnected, changed-while-reading and
            // deleted-on-its-share banners (ADR-0006, 1.1a, ADR-0009).
            // `refreshDriveState` drops rows read before a change (task
            // 1.9); the banner offers Reload.
            Logger.document.error("A background job ended: \(String(describing: error), privacy: .public)")
            if job == .index, current {
                // No more rows for this reading: no "Indexing…" for good.
                indexStopped = true
            }
            if let progress = call({ try $0.progress() }) {
                progressArrived(progress)
            } else {
                refreshDriveState()
                onChange?(.progress)
            }
            switch error as? JobFailure {
            case .DriveDisconnected? where !isShareBackedOff:
                // The drive may be back already, before the copy noticed it
                // had gone (a volume that mounts again quickly). Not for a
                // share that keeps failing at the same place: that is a bad
                // read, which would reconnect and fail for ever.
                checkOriginal()
            case .DriveDisconnected?:
                break
            case .DeletedElsewhere?:
                // The share said the file is gone: look at its path, so the
                // file's own state says so too (a share's watcher doesn't
                // see another computer's changes).
                checkOriginal()
            default:
                break
            }
        case .Failed?:
            Logger.document.error("A background job ended: \(String(describing: error), privacy: .public)")
            // Without the review there are only no suggestions. Without the
            // index, no more rows come: stop showing progress, show the rows
            // read, and say so.
            guard job == .index, current, !readStopped else { return }
            readStopped = true
            if let current = call({ try $0.progress() }), current.generation == generation {
                progress = current
            }
            refreshDriveState()
            onChange?(.progress)
        }
    }

    private func indexFinished(generation: UInt64, handle number: Int) {
        guard number == handleNumber, generation == self.generation, let progress = call({ try $0.progress() }) else { return }
        progressArrived(progress)
    }

    private func reviewFinished(generation: UInt64, handle number: Int) {
        guard number == handleNumber, generation == self.generation, let review = call({ try $0.review() }) else { return }
        self.review = review
        reviewedLineEnding = review?.lineEnding ?? reviewedLineEnding
        onChange?(.progress)
        if readingFromSave, review?.delimiterSuggestion != nil {
            onSavedReadingReviewed?()
        }
    }

    /// Reads where the bytes are, whether the file changed while read, and
    /// whether Save is possible (ADR-0006).
    private func refreshDriveState() {
        let changedBefore = changedOnDisk
        storage = call({ try $0.storage() }) ?? storage
        changedOnDisk = call({ try $0.changedOnDisk() }) ?? changedOnDisk
        canSave = call({ try $0.canSave() }) ?? canSave
        if storage == .disconnected {
            noteDisconnection()
        }
        recheckShareWhileDisconnected()
        if changedOnDisk, !changedBefore {
            // The file changed while it was read (1.1a): rows read before
            // the change was noticed may be from either version, so none
            // are kept. The core now serves only rows from its checked copy
            // (task 1.9), and the window redraws from those.
            tiles.removeAll()
            flagBlocks.removeAll()
            if let current = call({ try $0.progress() }), current.generation == generation {
                progress = current
            }
        }
    }

    /// While a file on a network share is disconnected, looks at it again
    /// every `shareRecheckInterval` (task 2.0 review): a share whose SMB
    /// session comes back by itself fires no mount notification, and the
    /// app may not be activated meanwhile. The check runs off the main
    /// thread (`checkOriginal`) and stops once the share is back or the
    /// document closes.
    /// The share failed at the same place `shareRecheckLimit` times in a
    /// row: Leal stops looking for it by itself.
    private var isShareBackedOff: Bool {
        (lastDisconnection?.times ?? 0) >= Self.shareRecheckLimit
    }

    /// The periodic check has stopped for a share that keeps failing at the
    /// same place, and no look at the file is under way: nothing will read
    /// it again by itself. For tests.
    var isShareSettledForTesting: Bool {
        isShareBackedOff && shareRecheck == nil && checkingOriginal == nil
    }

    private func recheckShareWhileDisconnected() {
        guard isOnNetworkShare, storage == .disconnected, shareRecheck == nil, failure == nil, !isShareBackedOff
        else { return }
        shareRecheck = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: DocumentModel.shareRecheckInterval)
                // The model only for this step: not held across the wait.
                guard let check = self?.shareRecheckStep() else { break }
                await check?.value
            }
            self?.shareRecheck = nil
        }
    }

    /// One step of the periodic share check: `nil` to stop (the share is
    /// back, the document failed, or the checks are backed off), otherwise
    /// the check started, if one wasn't already under way.
    private func shareRecheckStep() -> Task<Void, Never>?? {
        guard !Task.isCancelled, storage == .disconnected, failure == nil, !isShareBackedOff
        else { return nil }
        return .some(checkOriginal())
    }

    /// The share is disconnected: count it once for this reading (a
    /// reconnection reads the file again, as a new generation, and may fail
    /// again before the window has seen it reading), and in a row with the
    /// last if it stopped where that one did (a read that fails each time,
    /// not a share that comes and goes).
    private func noteDisconnection() {
        guard isOnNetworkShare, lastDisconnection?.generation != generation,
              let offset = call({ try $0.availableBytes() })
        else { return }
        if let last = lastDisconnection, last.offset == offset {
            lastDisconnection = (generation, offset, last.times + 1)
        } else {
            lastDisconnection = (generation, offset, 1)
        }
    }

    /// A mount, an app activation or a Reload: the periodic share check may
    /// run again, however often it failed.
    private func resetShareBackoff() {
        lastDisconnection = nil
        recheckShareWhileDisconnected()
    }

    /// A read failed because the drive went or the file changed: check the
    /// drive state soon, outside whatever is running now.
    private func queueDriveCheck() {
        guard !driveCheckQueued else { return }
        driveCheckQueued = true
        Task { @MainActor [weak self] in
            guard let self else { return }
            driveCheckQueued = false
            guard failure == nil, handle != nil else { return }
            refreshDriveState()
            onChange?(.progress)
        }
    }

    /// The latest diagnostics report, unless the one held is already the
    /// complete report of this reading.
    private func refreshDiagnostics() {
        if let held = diagnostics, held.generation == generation, held.complete { return }
        guard let report = call({ try $0.diagnostics() }), report.generation == generation else { return }
        if awaitingCompleteDiagnostics {
            // A save's reading: its `IndexProgress` was complete at once,
            // its diagnostics aren't until its index pass ends (task 2.4c).
            guard report.complete else { return }
            awaitingCompleteDiagnostics = false
        }
        diagnostics = report
        // The marks grew, and ragged rows may have changed with the mode.
        flagBlocks.removeAll()
    }

    /// A progress report from the relay for core document `number`.
    private func progressArrived(_ report: IndexProgress, handle number: Int) {
        guard number == handleNumber else { return }
        progressArrived(report)
    }

    /// A progress report from the relay (or a fresh one).
    private func progressArrived(_ report: IndexProgress) {
        guard failure == nil, report.generation == generation else { return }
        progress = report
        if let count = call({ try $0.columnCount() }) {
            fileColumnCount = Int(count)
        }
        // A removable drive's file moves from being read to a copy as the
        // index pass copies it, or is disconnected (ADR-0006).
        refreshDriveState()
        refreshDiagnostics()
        loadHeaderTitlesIfNeeded()
        updateColumnCount()
        let enough = Int(report.rows) >= Int(Self.sizingRows) + headerOffset
        if !refinedSizingStarted, enough || report.complete {
            startRefinedSizing()
        }
        onChange?(.progress)
    }

    // MARK: Grid data source

    var rowCount: Int {
        guard failure == nil else { return 0 }
        // After a read error the rows read are all there will be.
        if readStopped || indexStopped { return loadedRowCount }
        return max(0, Int(progress.estimatedRows) - headerOffset)
    }

    var loadedRowCount: Int {
        guard failure == nil else { return 0 }
        return max(0, Int(progress.rows) - headerOffset)
    }

    /// No more rows will come: the index is complete, or stopped on a read
    /// error. A pending ⌘↓ or Go to Row then goes to the last row read.
    var isIndexComplete: Bool { progress.complete || readStopped || indexStopped }

    func headerTitle(column: Int) -> HeaderTitle {
        if !interpretation.header {
            return HeaderTitle(text: String(column + 1), style: .number)
        }
        if column < headerTitles.count {
            return HeaderTitle(text: headerTitles[column], style: .name)
        }
        return HeaderTitle(text: GridStrings.extraColumn(column + 1), style: .extra)
    }

    func isNumeric(column: Int) -> Bool {
        column < numeric.count && numeric[column]
    }

    func cell(row: Int, column: Int) -> GridCell {
        tiles.cell(row: row, column: column, loadedRows: loadedRowCount)
    }

    /// A marker in the gutter: the row has a warning or an error (ADR-0002
    /// question 7), for every row, not only the report's first 1,000.
    func rowHasMarker(_ row: Int) -> Bool {
        flags(row: row).marked
    }

    /// A short row's missing cell is hatched once the core says the row is
    /// ragged (ADR-0002 question 5). Columns past the most common field
    /// count belong to longer rows, so they stay blank.
    func isHatched(row: Int, column: Int) -> Bool {
        column < fileColumnCount && flags(row: row).ragged
    }

    /// An edited cell carries a corner triangle until it is saved (mockup
    /// 05a): the core names the edited cells in each row the grid reads.
    func isEdited(row: Int, column: Int) -> Bool {
        failure == nil && hasUnsavedEdits && tiles.isEdited(row: row, column: column)
    }

    /// The grid row's flags, from the core one block at a time.
    private func flags(row: Int) -> RowFlags {
        guard failure == nil, row >= 0, row < loadedRowCount else { return RowFlags(marked: false, ragged: false) }
        let block = row / Self.flagBlockRows
        if flagBlocks[block] == nil {
            if flagBlocks.count > 64 { flagBlocks.removeAll() }
            let start = UInt64(block * Self.flagBlockRows + headerOffset)
            flagBlocks[block] = call({ try $0.rowFlags(start: start, count: UInt32(Self.flagBlockRows)) }) ?? []
        }
        let offset = row - block * Self.flagBlockRows
        guard let flags = flagBlocks[block], offset < flags.count else { return RowFlags(marked: false, ragged: false) }
        return flags[offset]
    }

    // MARK: Diagnostics navigation (mockup 03b)

    /// The first row of the file is the header row, which isn't a grid row.
    var headerRows: Int { headerOffset }

    /// The grid row a data row (physical row `row`, not the header row) is
    /// shown in.
    func gridRow(ofPhysical row: UInt64) -> Int {
        max(0, Int(row) - headerOffset)
    }

    /// The next (`forward`) occurrence of `kind` at or after physical row
    /// `from`, or the previous one before it. The core searches every row
    /// with the row marks, past the report's first 1,000 locations; it may
    /// read many rows, so it runs off the main actor. `nil` if there is
    /// none, or if the file was read again meanwhile.
    func find(_ kind: DiagnosticKind, forward: Bool, from: UInt64) async -> DiagnosticPlace? {
        guard failure == nil, let handle else { return nil }
        let reading = readingID
        coreCalls += 1
        let result = await Task.detached(priority: .userInitiated) { () -> Result<DiagnosticPlace?, any Error> in
            Result {
                forward
                    ? try handle.nextWithKind(kind: kind, from: from)
                    : try handle.previousWithKind(kind: kind, to: from)
            }
        }.value
        guard reading == readingID, failure == nil else { return nil }
        switch result {
        case let .success(place):
            return place
        case let .failure(error):
            // Handled as any core call's error: a failure fails the
            // document, a vanished drive is shown.
            let _: Void? = call { _ -> Void in throw error }
            return nil
        }
    }

    func cachedCell(row: Int, column: Int) -> GridCell? {
        tiles.cachedCell(row: row, column: column, loadedRows: loadedRowCount)
    }

    func readAhead(rows: Range<Int>, columns: Range<Int>) {
        tiles.readAhead(rows: rows, columns: columns, loadedRows: loadedRowCount)
    }

    func prepare(rows: Range<Int>, columns: Range<Int>) {
        tiles.prepare(rows: rows, columns: columns, loadedRows: loadedRowCount)
        if tiles.widestRow > columnCount, !columnUpdateScheduled {
            // A long row was read: it needs an extra column. Not while
            // drawing, though.
            columnUpdateScheduled = true
            Task { @MainActor [weak self] in
                guard let self else { return }
                columnUpdateScheduled = false
                updateColumnCount()
                onChange?(.columns)
            }
        }
    }

    private func readTile(rows: Range<Int>, columns: Range<Int>) -> [TileRow]? {
        let start = UInt64(rows.lowerBound + headerOffset)
        guard let read = call({
            try $0.cells(
                rowStart: start,
                rowCount: UInt32(rows.count),
                columnStart: UInt32(columns.lowerBound),
                columnCount: UInt32(columns.count),
                maxChars: GridMetrics.maxCellCharacters
            )
        }) else { return nil }
        return read.map(Self.tileRow)
    }

    /// A row the core read, as the tile cache keeps it.
    nonisolated private static func tileRow(_ row: RowCells) -> TileRow {
        TileRow(
            fieldCount: Int(row.fieldCount),
            cells: row.cells.map { .text($0.text, truncated: $0.truncated) },
            edited: row.edited.map(Int.init)
        )
    }

    /// `readTile` for the grid's reads ahead, off the main thread (task
    /// 2.0a), with the header offset of now. It reads from the core
    /// document of when it runs (`readAheadHandle`): none once the document
    /// has closed or failed. An error comes back to `report` through the
    /// tile cache. A read that ends holding the last reference to a closed
    /// document lets go of it through `CoreRelease`, as `close` would have.
    private func backgroundTileReader() -> CellTileCache.BackgroundFetch? {
        guard backgroundHandle() != nil else { return nil }
        let offset = headerOffset
        let current = readAheadHandle
        return { rows, columns in
            var handle = current.get()
            defer { CoreRelease.later(&handle) }
            return try handle?.cells(
                rowStart: UInt64(rows.lowerBound + offset),
                rowCount: UInt32(rows.count),
                columnStart: UInt32(columns.lowerBound),
                columnCount: UInt32(columns.count),
                maxChars: GridMetrics.maxCellCharacters
            ).map(Self.tileRow)
        }
    }

    /// SEAM(2.5): the values of `rows` (grid rows) changed: an edit, an
    /// undo or a redo. The tiles holding them are read again when drawn,
    /// and a read of them already under way is thrown away when it comes
    /// back (`CellTileCache.invalidate`); the window redraws them, in every
    /// strip that shows them (`.cells`, task 2.0b). See docs/tasks/2.0a.md,
    /// "For editing", for what else an edit must refresh.
    func cellsChanged(rows: Range<Int>) {
        tiles.invalidate(rows: rows)
        onChange?(.cells(rows))
    }

    // MARK: Edits (task 2.5.1)

    /// The cells `command` changed read differently now: an edit, or (task
    /// 2.5.2) an undo or a redo. Everything that shows them catches up
    /// (ADR-0008 decision 2; docs/tasks/2.0a.md, "For editing"):
    /// - the grid rows' tiles, strips and gutter marks (`cellsChanged`),
    ///   with the row flags read again (an edited cell's diagnostics are
    ///   its new value's, and a hatched cell edited is no longer missing);
    /// - the column headers, for file row 0 while it is the header row;
    /// - the column widths: a column widens for a value wider than it
    ///   (unless the user sized it), and an edit inside the sample rows
    ///   measures the widths and number detection again, off the main
    ///   thread, as after a reinterpret.
    ///
    /// Find (`FindModel.valuesChanged`) and the inspector hear of it from
    /// the window, through `.cells` and `.header`. `direction` says which
    /// values the cells read as now: after an undo, the old ones. Only
    /// `commandApplied` calls this.
    func valuesChanged(by command: EditCommand, direction: CommandDirection) {
        guard failure == nil, !command.changes.isEmpty else { return }
        var gridRows: [Int] = []
        var header = false
        for change in command.changes {
            if interpretation.header, change.row == 0 {
                header = true
            } else {
                gridRows.append(Int(clamping: change.row) - headerOffset)
            }
        }
        if header { reloadHeaderTitles() }
        if let first = gridRows.min(), let last = gridRows.max() {
            for block in (first / Self.flagBlockRows)...(last / Self.flagBlockRows) {
                flagBlocks[block] = nil
            }
            cellsChanged(rows: first..<(last + 1))
        }
        widenColumns(for: command.changes, direction: direction)
        let sampled = UInt64(headerOffset) + UInt64(Self.sizingRows)
        if refinedSizingStarted, command.changes.contains(where: { $0.row < sampled }) {
            measureAgainAfterEdit()
        }
    }

    /// Rows or a column were inserted or deleted (task 2.5.2: an undo or a
    /// redo of one; inserting and deleting them is task 2.5a). Every row
    /// after the change moved, so every tile and row flag is read again;
    /// the row count comes from the core. A column moves the widths after
    /// it, and the titles, column count and sample are read again.
    func structureChanged(by structural: StructuralEdit, direction: CommandDirection) {
        guard failure == nil else { return }
        flagBlocks.removeAll()
        tiles.removeAll()
        if let current = call({ try $0.progress() }) { progress = current }
        if let count = call({ try $0.columnCount() }) { fileColumnCount = Int(count) }
        if structural.isColumn(), let at = structural.column().map(Int.init) {
            // An undo inserts what a delete took, and the reverse.
            let inserted = structural.inserts() == (direction != .undo)
            moveColumns(at: at, inserted: inserted)
            // The sample's widest row is measured again, if at all.
            widestSampleRow = 0
            if interpretation.header {
                reloadHeaderTitles()
            }
            updateColumnCount()
            if refinedSizingStarted { measureAgainAfterEdit() }
        }
        onChange?(.structure)
    }

    /// A column was inserted at `at`, or deleted there: the widths and
    /// what is known of each column after it move with it.
    private func moveColumns(at: Int, inserted: Bool) {
        func move<T>(_ values: inout [T], filler: T) {
            if inserted {
                if at <= values.count { values.insert(filler, at: at) }
            } else if at < values.count {
                values.remove(at: at)
            }
        }
        move(&columnWidths, filler: GridMetrics.defaultColumnWidth)
        move(&widestText, filler: 0)
        move(&numeric, filler: false)
        let shift = inserted ? 1 : -1
        resizedColumns = Set(resizedColumns.compactMap { column in
            column < at ? column : (!inserted && column == at ? nil : column + shift)
        })
        editedWidest = Dictionary(uniqueKeysWithValues: editedWidest.compactMap { column, width in
            column < at ? (column, width) : (!inserted && column == at ? nil : (column + shift, width))
        })
        columnCount = 0
    }

    /// The header row's titles, read again after an edit to it.
    private func reloadHeaderTitles() {
        let columns = UInt32(max(columnCount, fileColumnCount, 1))
        guard let row = call({
            try $0.cells(rowStart: 0, rowCount: 1, columnStart: 0, columnCount: columns, maxChars: GridMetrics.maxCellCharacters)
        })?.first else { return }
        headerTitles = row.cells.map { CellText.display($0.text).text }
        updateColumnCount()
        onChange?(.header)
    }

    /// Widens each changed cell's column to fit the value it reads as now
    /// (its new value, or after an undo its old one), as the sizing would
    /// have (up to `GridMetrics.maximumColumnWidth`), unless the user sized
    /// the column. A column never narrows here: that waits for the sample
    /// to be measured again.
    private func widenColumns(for changes: [ValueChange], direction: CommandDirection) {
        var widened = false
        for change in changes {
            let column = Int(change.column)
            let now = direction == .undo ? change.oldValue : change.newValue
            guard column < columnWidths.count, column < widestText.count, let value = now else { continue }
            let limit = Int(GridMetrics.maxCellCharacters)
            let start = value.unicodeScalars.prefix(limit + 1)
            let truncated = start.count > limit
            let shown = String(String.UnicodeScalarView(start.prefix(limit)))
            let measured: CGFloat = if interpretation.header, change.row == 0 {
                headerMeasurer.width(of: CellText.display(shown).text)
            } else {
                Self.cellMeasure(numeric: numeric, cell: cellMeasurer, number: numberMeasurer)(column, shown, truncated)
            }
            let width = min(measured, GridMetrics.fitMaximumWidth)
            if change.row >= UInt64(headerOffset) + UInt64(Self.sizingRows) {
                editedWidest[column] = max(editedWidest[column] ?? 0, width)
            }
            guard width > widestText[column] else { continue }
            widestText[column] = width
            guard !resizedColumns.contains(column), let fitted = ColumnSizer.widths(fromWidest: [width]).first,
                  fitted > columnWidths[column] else { continue }
            columnWidths[column] = fitted
            widened = true
        }
        if widened { onChange?(.widths) }
    }

    /// Measures the sample rows' widths and number detection again, off
    /// the main thread, a moment after the last edit inside them (so a run
    /// of edits measures once).
    private func measureAgainAfterEdit() {
        sizingAfterEdit?.cancel()
        let reading = readingID
        sizingAfterEdit = Task { [weak self] in
            try? await Task.sleep(for: Self.sizingAfterEditDelay)
            guard !Task.isCancelled, let self, readingID == reading, failure == nil else { return }
            sizingAfterEdit = nil
            remeasuring = true
            startRefinedSizing()
        }
    }

    /// How long after an edit the sample is measured again. Tests shorten it.
    static var sizingAfterEditDelay: Duration = .milliseconds(300)

    /// Whether a measuring after an edit is waiting or running, for tests.
    var isMeasuringAfterEdit: Bool { sizingAfterEdit != nil || remeasuring }

    /// How many of the grid's reads ahead have come back, kept or dropped,
    /// for tests.
    var readsAheadBack: Int { tiles.readsAheadBack }
    var readsAheadStarted: Int { tiles.readsAheadStarted }

    // MARK: Columns

    /// How many rows a sizing read takes for `columns` columns: `wanted`,
    /// or fewer, so that it reads at most `sizingFieldLimit` fields.
    nonisolated static func sizingRows(wanted: UInt32, columns: Int) -> UInt32 {
        let fit = sizingFieldLimit / max(1, columns)
        return UInt32(max(1, min(Int(wanted), fit)))
    }

    /// Sizes the columns from the first screen's rows (P0) and asks the core
    /// which are numeric, from the same rows. The first screen gave the first
    /// row (the header titles); the rows are read here, capped in fields.
    private func applyFirstScreen(_ screen: FirstScreen) {
        headerTitles = []
        if interpretation.header, let first = screen.rows.first {
            headerTitles = first.map { CellText.display($0.text).text }
        }
        let columns = max(fileColumnCount, headerTitles.count, screen.rows.first?.count ?? 0, 1)
        let wanted = Self.sizingRows(wanted: Self.firstScreenRows, columns: columns)
        let rows = readSizingRows(count: wanted, columns: columns)
        numeric = call({ try $0.numericColumns(sample: wanted) }) ?? []
        widestSampleRow = rows.fieldCount
        updateColumnCount()
        widestText = measure(rows: rows.cells, columns: columnCount)
        columnWidths = ColumnSizer.widths(fromWidest: widestText)
    }

    /// The sizing rows after the header, as (text, truncated), with the most
    /// fields any of them has.
    private func readSizingRows(count: UInt32, columns: Int) -> (cells: [[(text: String, truncated: Bool)]], fieldCount: Int) {
        largestSizingRead = max(largestSizingRead, Int(count) * columns)
        let start = UInt64(headerOffset)
        let read = call {
            try $0.cells(
                rowStart: start,
                rowCount: count,
                columnStart: 0,
                columnCount: UInt32(columns),
                maxChars: GridMetrics.maxCellCharacters
            )
        } ?? []
        return (
            read.map { $0.cells.map { (text: $0.text, truncated: $0.truncated) } },
            read.map { Int($0.fieldCount) }.max() ?? 0
        )
    }

    /// A header row longer than the first 64 KB isn't in the first screen:
    /// read its titles once the index has it.
    private func loadHeaderTitlesIfNeeded() {
        guard interpretation.header, headerTitles.isEmpty, progress.rows > 0 else { return }
        let columns = UInt32(max(fileColumnCount, 1))
        guard let row = call({ try $0.cells(rowStart: 0, rowCount: 1, columnStart: 0, columnCount: columns, maxChars: GridMetrics.maxCellCharacters) })?.first else { return }
        headerTitles = row.cells.map { CellText.display($0.text).text }
        onChange?(.columns)
    }

    /// The grid's column count: the most common field count, or more if the
    /// header or a row read so far is longer.
    private func updateColumnCount() {
        let count = max(fileColumnCount, headerTitles.count, widestSampleRow, tiles?.widestRow ?? 0)
        guard count != columnCount else { return }
        columnCount = count
        if columnWidths.count < count {
            columnWidths += Array(repeating: GridMetrics.defaultColumnWidth, count: count - columnWidths.count)
        }
        if widestText.count < count {
            widestText += Array(repeating: 0, count: count - widestText.count)
        }
    }

    /// Each column's widest text in `rows` and the header, up to the widest
    /// a double-click can make a column.
    private func measure(rows: [[(text: String, truncated: Bool)]], columns: Int) -> [CGFloat] {
        let header = headerMeasurer
        return ColumnSizer.widest(
            columns: columns,
            header: interpretation.header ? headerTitles : [],
            rows: rows,
            limit: GridMetrics.fitMaximumWidth,
            measureCell: Self.cellMeasure(numeric: numeric, cell: cellMeasurer, number: numberMeasurer),
            measureHeader: { header.width(of: $0) }
        )
    }

    /// Measures a cell in the font the grid draws its column in.
    nonisolated private static func cellMeasure(
        numeric: [Bool],
        cell: TextMeasurer,
        number: TextMeasurer
    ) -> @Sendable (Int, String, Bool) -> CGFloat {
        { column, text, truncated in
            let isNumber = column < numeric.count && numeric[column]
            return (isNumber ? number : cell).cellWidth(of: text, truncated: truncated)
        }
    }

    /// P2 (DESIGN §3.10): widths and number detection from the first 1,000
    /// rows (fewer for a very wide file), off the main thread, once the
    /// index has them.
    private func startRefinedSizing() {
        guard let handle, failure == nil else { return }
        refinedSizingStarted = true
        let reading = readingID
        let first = UInt64(headerOffset)
        let columns = max(columnCount, 1)
        let rowCount = Self.sizingRows(wanted: Self.sizingRows, columns: columns)
        largestSizingRead = max(largestSizingRead, Int(rowCount) * columns)
        let cell = cellMeasurer
        let number = numberMeasurer
        let header = headerMeasurer
        let titles = interpretation.header ? headerTitles : []
        let batch = UInt32(max(1, Self.sizingBatchFields / columns))
        let task = Task.detached(priority: .utility) { [weak self] in
            // Off the main thread: reads from the core (each well under a
            // millisecond a hundred rows) and the measuring. Only each
            // column's widest text is kept. The rows come in batches of at
            // most `sizingBatchFields` fields: the core holds the reading's
            // row cache while it reads, and the grid's reads on the main
            // thread wait for it, so each hold is kept short (phase 1
            // review, app-10).
            let result: Result<RefinedColumns, any Error>
            do {
                var rows: [RowCells] = []
                var next = first
                while next < first + UInt64(rowCount), !Task.isCancelled {
                    let count = UInt32(min(UInt64(batch), first + UInt64(rowCount) - next))
                    let read = try handle.cells(
                        rowStart: next,
                        rowCount: count,
                        columnStart: 0,
                        columnCount: UInt32(columns),
                        maxChars: GridMetrics.maxCellCharacters
                    )
                    rows += read
                    // Fewer rows than asked for: the index has no more yet.
                    if read.count < Int(count) { break }
                    next += UInt64(count)
                }
                let numeric = try handle.numericColumns(sample: rowCount)
                let widest = ColumnSizer.widest(
                    columns: columns,
                    header: titles,
                    rows: rows.map { $0.cells.map { (text: $0.text, truncated: $0.truncated) } },
                    limit: GridMetrics.fitMaximumWidth,
                    measureCell: Self.cellMeasure(numeric: numeric, cell: cell, number: number),
                    measureHeader: { header.width(of: $0) }
                )
                let fieldCount = rows.map { Int($0.fieldCount) }.max() ?? 0
                result = .success(RefinedColumns(widest: widest, numeric: numeric, fieldCount: fieldCount))
            } catch {
                result = .failure(error)
            }
            await self?.applyRefinedSizing(result, reading: reading)
        }
        keepUntilDone(task)
    }

    /// Keeps `task` (for cancelling) until it ends, then forgets it, so a
    /// task started again after each edit doesn't pile up.
    private func keepUntilDone(_ task: Task<Void, Never>) {
        tasks.append(task)
        Task { [weak self] in
            await task.value
            self?.tasks.removeAll { $0 == task }
        }
    }

    private func applyRefinedSizing(_ result: Result<RefinedColumns, any Error>, reading: ReadingID) {
        guard reading == readingID, failure == nil else { return }
        let again = remeasuring
        remeasuring = false
        switch result {
        case let .success(refined):
            let before = (numeric: numeric, widths: columnWidths, count: columnCount)
            numeric = refined.numeric
            widestSampleRow = max(widestSampleRow, refined.fieldCount)
            updateColumnCount()
            // An edited value outside the sample rows keeps its column as
            // wide as it made it (task 2.5.1).
            let widest = refined.widest.enumerated().map { max($1, editedWidest[$0] ?? 0) }
            for (column, widest) in widest.enumerated() where column < widestText.count {
                widestText[column] = widest
            }
            let widths = ColumnSizer.widths(fromWidest: widest)
            for (column, width) in widths.enumerated() where column < columnWidths.count && !resizedColumns.contains(column) {
                columnWidths[column] = width
            }
            isSizingRefined = true
            // Measured again after an edit (task 2.5.1): redrawn only if
            // something changed, since `.columns` lays out every line again.
            if again, before == (numeric, columnWidths, columnCount) { return }
            onChange?(.columns)
        case let .failure(error):
            switch error as? LealError {
            case .DocumentFailed?, nil:
                fail(error)
            case .some:
                // Rows that couldn't be read (a removable drive that
                // vanished): keep the first screen's widths.
                Logger.document.error("Column sizing couldn’t read rows: \(String(describing: error), privacy: .public)")
            }
        }
    }

    /// The user resized a column: sizing leaves it alone from now on.
    func columnResized(_ column: Int, width: CGFloat) {
        guard column < columnWidths.count else { return }
        resizedColumns.insert(column)
        columnWidths[column] = width
    }

    /// The width that fits column `column`'s contents in the sizing rows and
    /// in `visibleRows`, for a double-click on its header edge.
    func fittingWidth(column: Int, visibleRows: Range<Int>) -> CGFloat? {
        guard column < columnCount else { return nil }
        var rows: [[(text: String, truncated: Bool)]] = []
        for row in visibleRows {
            if case let .text(text, truncated) = cell(row: row, column: column) {
                rows.append([(text, truncated)])
            }
        }
        let font = isNumeric(column: column) ? numberMeasurer : cellMeasurer
        let visible = ColumnSizer.widest(
            columns: 1,
            header: [],
            rows: rows,
            limit: GridMetrics.fitMaximumWidth,
            measureCell: { _, text, truncated in font.cellWidth(of: text, truncated: truncated) },
            measureHeader: { _ in 0 }
        ).first ?? 0
        let sampled = column < widestText.count ? widestText[column] : 0
        return ColumnSizer.widths(fromWidest: [max(visible, sampled)], maximum: GridMetrics.fitMaximumWidth).first
    }

    // MARK: Re-reading

    /// The Header row toggle (ADR-0002 question 13): reads the file again
    /// with the first row as a header row or not, keeping the user's other
    /// choices. Nothing is reopened (PLAN 1.3).
    func setHeaderRow(_ header: Bool) {
        guard header != interpretation.header else { return }
        reinterpret(header: header)
    }

    /// **Treat as** (ADR-0005 decision 8): reads the file again with
    /// `delimiter`, keeping the user's other choices. The file is
    /// re-indexed, and its diagnostics start again; no byte changes.
    func treatAs(_ delimiter: Delimiter) {
        guard delimiter != interpretation.delimiter else { return }
        reinterpret(delimiter: delimiter)
    }

    /// **Reopen with encoding** (ADR-0005 decision 8): reads the file again
    /// in `encoding`, in place of the guess, the BOM's or the file's
    /// attribute's (ADR-0004 decision 11). Only the encodings the core
    /// offers (`encodingChoices`) fit the file.
    func reopen(encoding: TextEncoding) {
        guard encoding != interpretation.encoding || interpretation.encodingSource != .user else { return }
        reinterpret(encoding: encoding)
    }

    /// Whether the file may be read again another way (Treat As, Reopen
    /// with Encoding, the Header row): not once it changed while it was
    /// read, when the first 64 KB Leal holds may be from the old version.
    /// The core refuses then (`ChangedOnDisk`); the window turns the three
    /// off and says to Reload first. Nor while a Reload or a save runs (the
    /// core refuses with `Saving`).
    var canReinterpret: Bool { failure == nil && !changedOnDisk && !isReloading && !isSaving }

    /// Whether the file may be read with another delimiter or encoding
    /// (Treat As, Reopen with Encoding): as `canReinterpret`, and not while
    /// there are unsaved edits, which are tied to how the file was split
    /// (ADR-0008 decision 4). The Header row toggle stays available.
    var canChangeSplit: Bool { canReinterpret && !hasUnsavedEdits }

    /// Reads the core's dirty state again.
    func refreshUnsavedEdits() {
        guard failure == nil else { return }
        hasUnsavedEdits = call { try $0.hasUnsavedEdits() } ?? hasUnsavedEdits
    }

    /// Reads the file again with the given choices, keeping the user's
    /// earlier ones for the rest. Nothing is reopened (PLAN 1.3).
    private func reinterpret(delimiter: Delimiter? = nil, header: Bool? = nil, encoding: TextEncoding? = nil) {
        guard canReinterpret else { return }
        // Edits are tied to the split (ADR-0008 decision 4): the core
        // refuses another delimiter or encoding while there are any.
        if delimiter != nil || encoding != nil, hasUnsavedEdits { return }
        let lineage = call { try $0.lineage() }
        let current = interpretation
        let options = OpenOptions(
            delimiter: delimiter ?? (current.delimiterSource == .user ? current.delimiter : nil),
            header: header ?? (current.headerSource == .user ? current.header : nil),
            encoding: encoding ?? (current.encodingSource == .user ? current.encoding : nil),
            firstScreenRows: 1,
            maxChars: GridMetrics.maxCellCharacters
        )
        guard let screen = call({ try $0.reinterpret(options: options) }) else { return }
        for task in tasks { task.cancel() }
        tasks.removeAll()
        interpretation = screen.interpretation
        generation = screen.generation
        readingFromSave = false
        awaitingCompleteDiagnostics = false
        fileColumnCount = Int(screen.columnCount)
        reviewedLineEnding = nil
        review = nil
        diagnostics = nil
        readStopped = false
        indexStopped = false
        flagBlocks.removeAll()
        tiles.removeAll()
        resizedColumns.removeAll()
        editedWidest.removeAll()
        columnWidths = []
        widestText = []
        widestSampleRow = 0
        columnCount = 0
        refinedSizingStarted = false
        remeasuring = false
        isSizingRefined = false
        if let current = call({ try $0.progress() }) { progress = current }
        applyFirstScreen(screen)
        refreshUnsavedEdits()
        let split = call { try $0.lineage() } == lineage
        onReadingChanged?(split ? .sameSplit : .newSplit)
        onChange?(.content)
        startWaiting()
        progressArrived(progress)
    }

    // MARK: The user's file (task 1.9)

    /// Asks the core to report changes to the file, through a relay that
    /// ignores reports about an older core document.
    private func watchOriginal() {
        let reference = ModelReference()
        reference.model = self
        let number = handleNumber
        let relay = OriginalRelay { status in reference.model?.originalArrived(status, handle: number) }
        _ = call { try $0.watchOriginal(observer: relay) }
    }

    /// Looks at the file again when a volume mounts or unmounts (a
    /// removable drive coming back, ADR-0006) and when Leal becomes active
    /// (a change the watcher can't see, such as a file put back at a
    /// deleted file's path while Leal was in the background).
    private func observeVolumesAndActivation() {
        guard observers.isEmpty else { return }
        let workspace = Self.notificationCentersForTesting?.workspace ?? NSWorkspace.shared.notificationCenter
        let app = Self.notificationCentersForTesting?.app ?? NotificationCenter.default
        let names = [
            (workspace, NSWorkspace.didMountNotification),
            (workspace, NSWorkspace.didUnmountNotification),
            (app, NSApplication.didBecomeActiveNotification),
        ]
        for (center, name) in names {
            let observer = center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?.resetShareBackoff()
                    _ = self?.checkOriginal()
                }
            }
            observers.append((center, observer))
        }
    }

    private func stopObserving() {
        for (center, observer) in observers { center.removeObserver(observer) }
        observers.removeAll()
        checkingOriginal?.cancel()
        checkingOriginal = nil
        checkAgain = false
    }

    private func originalArrived(_ status: OriginalStatus, handle number: Int) {
        guard number == handleNumber, failure == nil else { return }
        apply(original: status)
    }

    /// The file is now as `status` says: follow a move, and update what
    /// Save may do.
    private func apply(original status: OriginalStatus) {
        let moved = status.path != original.path
        original = status
        // A file moved to the Trash is deleted, as far as the window says;
        // the document keeps its place. A path in a temporary-items folder
        // is a safe save's staging area, never the document's new home (the
        // core doesn't report one either).
        let staging = URL(filePath: status.path).pathComponents.contains { $0 == "TemporaryItems" || $0 == ".TemporaryItems" }
        if moved, status.state != .deleted, !staging {
            url = URL(filePath: status.path)
            onMoved?(url)
        }
        refreshDriveState()
        onChange?(.progress)
    }

    /// Looks at the file now, off the main thread (it can block on a
    /// network volume): a removable drive that is back reconnects, and the
    /// core reads the file again to carry on copying it. Returns the task,
    /// for tests; `nil` if one is already under way, in which case another
    /// runs after it.
    @discardableResult
    func checkOriginal() -> Task<Void, Never>? {
        guard failure == nil, let handle else { return nil }
        guard checkingOriginal == nil else {
            checkAgain = true
            return nil
        }
        let number = handleNumber
        coreCalls += 1
        let task = Task { [weak self] in
            // On `FileWork`'s queue: a look at a share can block. At utility
            // QoS, as the watching thread it may wait for.
            let result = await FileWork.run(qos: .utility) { () -> Result<OriginalStatus, any Error> in
                Result { try handle.checkOriginal() }
            }
            // After a Reload, `checkingOriginal` is the new handle's.
            guard let self, number == handleNumber else { return }
            checkingOriginal = nil
            guard failure == nil else { return }
            switch result {
            case let .success(status):
                if let current = call({ try $0.progress() }), current.generation != generation {
                    restarted(current)
                }
                apply(original: status)
            case let .failure(error):
                // Handled as any core call's error.
                let _: Void? = call { _ -> Void in throw error }
            }
            if checkAgain {
                checkAgain = false
                checkOriginal()
            }
        }
        checkingOriginal = task
        return task
    }

    /// The core read the file again the same way (its drive came back):
    /// new jobs and diagnostics, the same interpretation and columns.
    private func restarted(_ report: IndexProgress) {
        for task in tasks { task.cancel() }
        tasks.removeAll()
        generation = report.generation
        readingFromSave = false
        awaitingCompleteDiagnostics = false
        progress = report
        diagnostics = nil
        review = nil
        readStopped = false
        indexStopped = false
        reviewedLineEnding = nil
        flagBlocks.removeAll()
        tiles.removeAll()
        refinedSizingStarted = false
        remeasuring = false
        isSizingRefined = false
        refreshDriveState()
        onChange?(.rows)
        startWaiting()
        progressArrived(report)
    }

    /// **Reload** (task 1.9): opens the file again through the core, so
    /// the window shows it as it is now. The user's own choices (Treat As,
    /// Reopen with Encoding, the Header row) are kept where they still fit
    /// the file, and so are the widths of columns they resized. Every
    /// cache is dropped. The old core document is released, which deletes
    /// its snapshot.
    ///
    /// Throws the open error if the file can't be opened (deleted, moved
    /// out of reach, or its drive away); the document is then unchanged.
    ///
    /// `url` is where to read the file from, if not where the model has
    /// it: `CSVDocument` passes the URL AppKit reads from. The model then
    /// lives there. Call it through `CSVDocument.reload`, which keeps
    /// `NSDocument`'s own idea of the file in step.
    func reload(from url: URL? = nil) throws {
        guard failure == nil else { return }
        let url = url ?? self.url
        let number = handleNumber + 1
        let reference = ModelReference()
        let new = try Self.openReloaded(url: url, environment: environment, options: reloadOptions, reference: reference, number: number)
        try adoptReloaded(new, url: url, reference: reference, number: number)
    }

    /// **Reload** as the window does it (task 2.0): as `reload`, but the
    /// core opens the file on `FileWork`'s queue, because first paint reads
    /// the file, which on a network share can block (ADR-0009). The window
    /// keeps showing the old snapshot until the new one is ready, and the
    /// re-readings (Reload, Treat As, Reopen with Encoding, the Header row)
    /// are off meanwhile (`isReloading`). Returns whether the new document
    /// was adopted: not if the model failed or closed meanwhile.
    ///
    /// Editing is off meanwhile (the window's `isReplacingDocument`), but
    /// as a second guard the new document isn't adopted if the edit
    /// version moved past `version` (by default, the one when the Reload
    /// was asked for, `willReload`): adopting it would throw away edits
    /// the user made since, and mark the document clean.
    ///
    /// - Throws: the open error, or `EditedDuringReload` if it wasn't
    ///   adopted because of edits made meanwhile, which are kept.
    @discardableResult
    func reloadInBackground(from url: URL? = nil, editVersion version: UInt64? = nil) async throws -> Bool {
        guard failure == nil else { return false }
        let url = url ?? self.url
        let number = handleNumber + 1
        let reference = ModelReference()
        let environment = environment
        let options = reloadOptions
        willReload()
        let since = version ?? reloadVersion ?? editVersion
        defer { reloadEnded() }
        var new: LealFFI.Document? = try await FileWork.run {
            try Self.openReloaded(url: url, environment: environment, options: options, reference: reference, number: number)
        }
        // The checks come before any binding of `new`: `CoreRelease` must
        // drop the last reference, off the main thread (its deinit deletes
        // a snapshot file, which can block on a share). A `let opened = new`
        // in the guard would keep one alive here.
        guard failure == nil, handle != nil, number == handleNumber + 1 else {
            CoreRelease.later(&new)
            return false
        }
        guard editVersion == since else {
            CoreRelease.later(&new)
            throw EditedDuringReload()
        }
        guard let opened = new.take() else { return false }
        try adoptReloaded(opened, url: url, reference: reference, number: number)
        return true
    }

    /// **Save As UTF-8** (task 2.3, ADR-0008 decision 7): the core writes
    /// the document to `url` in UTF-8, on a thread of its own, in a new
    /// folder on `url`'s volume that `FileManager` makes (which a sandboxed
    /// app may write to), and then reads that file. The main thread never
    /// waits for it (DESIGN §3.9); cancelling the task cancels the save.
    /// Returns `nil` if the document has failed or closed.
    ///
    /// - Throws: the `SaveFailure` the save ended with, or why the core
    /// wouldn't start it, as one: `DocumentFailed` (the document fails, as
    /// for any core call) or `Internal`.
    func saveAsUTF8(to url: URL) async throws -> SaveOutcome? {
        try await save(to: url, kind: .saveAsUtf8)?.outcome
    }

    // MARK: Saving (task 2.5.3a)

    /// A finished save: its outcome, and the edit version its snapshot of
    /// the edits was taken at (the edits up to it are in the file).
    struct Saved: Sendable {
        let outcome: SaveOutcome
        let snapshotVersion: UInt64?
    }

    /// Where a save writes its new file and snapshot: the folders
    /// `FileManager` made on the destination's volume, and what is known
    /// about that volume (`placeForSaving(_:)`).
    struct SavePlace: Sendable {
        let folder: String?
        let volume: VolumeInfo

        /// Removes the empty folders the save was given and didn't take
        /// over (it refused before writing), off the main thread. A folder
        /// the core took over is gone already, or holds a file it kept:
        /// `rmdir` leaves anything that isn't empty.
        func removeLeftovers() {
            let folders = [folder, volume.folder].compactMap { $0 }
            guard !folders.isEmpty else { return }
            FileWork.queue.async(qos: .utility) {
                for folder in folders { _ = rmdir(folder) }
            }
        }
    }

    /// The save under way (Save, or Save As UTF-8): while it runs the file
    /// isn't read again (Reload, Treat As, Reopen with Encoding and the
    /// Header row are off, `canReinterpret`), and the status bar shows how
    /// far it has got (`saveProgress`).
    private(set) var saveJob: SaveJob?
    /// The save's progress as last looked at (every `savePollInterval`),
    /// for the status bar; `nil` before the first look.
    private(set) var saveProgress: SaveProgress?
    /// A Save waits to start: for its turn at the file, for other apps to
    /// let go of it (coordination), or for another app's save to settle
    /// (`Moving`). The status bar says so ("Waiting to save…").
    private(set) var isWaitingToSave = false
    /// Looks at the save's progress, while it runs.
    private var saveWatching: Task<Void, Never>?
    /// How often the status bar's save progress is looked at.
    nonisolated static let savePollInterval: Duration = .milliseconds(100)
    var isSaving: Bool { saveJob != nil || isWaitingToSave }

    /// Saves through the core (ADR-0012): `kind` to `url`. The core writes
    /// on a thread of its own and the main thread never waits for it
    /// (DESIGN §3.9); cancelling the task cancels the save
    /// (`SaveJob.outcome()`). Edits carry on meanwhile.
    ///
    /// The new file is written in an item-replacement folder `FileManager`
    /// makes on `url`'s volume, which a sandboxed app may write to; a
    /// second one is for the snapshot of the saved file. Where it can't
    /// make one (some shares, FAT), or makes one on another volume, the
    /// core falls back to a hidden folder of its own next to the file, and
    /// for the snapshot to a copy on the internal disk (a share never gets
    /// one: `TemporaryFolders.volume(for:)`). Folders the core didn't take
    /// over (a refusal before writing) are removed afterwards.
    ///
    /// Save itself (`CSVDocument`) uses the parts below, so that its file
    /// access ends off the main thread: `placeForSaving`, `startSave`,
    /// `outcome(of:)` and `saveEnded`.
    ///
    /// Returns `nil` if the document has failed or closed.
    ///
    /// - Throws: the `SaveFailure` the save ended with, or why the core
    ///   wouldn't start it, as one: `DocumentFailed` (the document fails, as
    ///   for any core call) or `Internal`.
    func save(to url: URL, kind: SaveKind, overwriteChanged: Bool = false) async throws -> Saved? {
        guard failure == nil, handle != nil else { return nil }
        let place = await Self.placeForSaving(url)
        guard let job = try startSave(to: url, kind: kind, place: place, overwriteChanged: overwriteChanged).get() else {
            place.removeLeftovers()
            return nil
        }
        let result = await Self.outcome(of: job)
        saveEnded(job, place: place)
        return try result.get()
    }

    /// The folders a save to `url` writes in (`save(to:kind:)`), made off
    /// the main thread: asking `FileManager` about a volume can block (a
    /// share).
    nonisolated static func placeForSaving(_ url: URL) async -> SavePlace {
        await FileWork.run {
            SavePlace(
                folder: TemporaryFolders.volumeFolder(for: url),
                volume: TemporaryFolders.volume(for: url.deletingLastPathComponent())
            )
        }
    }

    /// Starts the core's save, and shows its progress until `saveEnded`.
    /// `nil` if the document has failed or closed.
    func startSave(to url: URL, kind: SaveKind, place: SavePlace, overwriteChanged: Bool) -> Result<SaveJob?, SaveFailure> {
        guard failure == nil, let handle else { return .success(nil) }
        let options = SaveOptions(
            destination: url.path(percentEncoded: false),
            kind: kind,
            folder: place.folder,
            volume: place.volume,
            overwriteChanged: overwriteChanged,
            firstScreenRows: 1,
            maxChars: GridMetrics.maxCellCharacters
        )
        coreCalls += 1
        let job: SaveJob
        do {
            job = try handle.save(options: options)
        } catch {
            report(error)
            if case let .DocumentFailed(_, message)? = error as? LealError {
                return .failure(.DocumentFailed(message: message))
            }
            return .failure(.Internal(message: String(describing: error)))
        }
        saveJob = job
        saveProgress = nil
        saveWatching?.cancel()
        saveWatching = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.savePollInterval)
                guard let self, saveJob === job, !Task.isCancelled else { return }
                saveProgress = job.progress()
                onChange?(.progress)
            }
        }
        onChange?(.progress)
        return .success(job)
    }

    /// Waits for `job`, off the main thread, and cancels it if the waiting
    /// task is cancelled (`SaveJob.outcome()`): what it saved, or why not.
    nonisolated static func outcome(of job: SaveJob) async -> Result<Saved, SaveFailure> {
        do {
            let outcome = try await job.outcome()
            return .success(Saved(outcome: outcome, snapshotVersion: job.progress().snapshotVersion))
        } catch let failure as SaveFailure {
            return .failure(failure)
        } catch {
            return .failure(.Internal(message: String(describing: error)))
        }
    }

    /// The save `job` ended: its progress goes from the status bar, and
    /// the folders it didn't take over are removed.
    func saveEnded(_ job: SaveJob, place: SavePlace) {
        place.removeLeftovers()
        guard saveJob === job else { return }
        saveWatching?.cancel()
        saveWatching = nil
        saveJob = nil
        saveProgress = nil
        onChange?(.progress)
    }

    /// Save is waiting to start (`isWaitingToSave`), or no longer is.
    func waitingToSave(_ waiting: Bool) {
        guard isWaitingToSave != waiting else { return }
        isWaitingToSave = waiting
        onChange?(.progress)
    }

    /// After a save that wrote the file: the dirty state is the core's
    /// again (edits made during the save are still unsaved), and the file
    /// is as the watcher sees it now, so `diverged` no longer asks.
    ///
    ///
    /// The core now reads the file it wrote (ADR-0008 decision 1), as a new
    /// reading: the model adopts it (`adoptSaved`). Not if it couldn't read
    /// the file back (`firstScreen` is `nil`): it still reads the old
    /// snapshot, with the edits.
    func saved(_ outcome: SaveOutcome) {
        guard failure == nil else { return }
        // Not `apply(original:)`: the path the core reports is the one it
        // wrote, a symbolic link followed, which isn't a move.
        original = OriginalStatus(state: outcome.original.state, path: original.path, diverged: outcome.original.diverged)
        refreshUnsavedEdits()
        if outcome.firstScreen != nil, let current = call({ try $0.progress() }), current.generation != generation {
            adoptSaved(current)
        } else {
            refreshDriveState()
            onChange?(.progress)
        }
    }

    /// Adopts the reading a save made of the file it wrote (task 2.5.3b), as
    /// after a re-read (`restarted`): its generation, so nothing read from
    /// the old one is used (tiles, with their edited-cell marks, and row
    /// flags), and its jobs, whose diagnostics and review are awaited
    /// afresh. The diagnostics are taken only once complete
    /// (`awaitingCompleteDiagnostics`). The interpretation, columns, their
    /// widths and the undo stack stay: the values are the same. If the
    /// core read the file again as the save ended (a drive back), this is
    /// that reading.
    private func adoptSaved(_ report: IndexProgress) {
        generation = report.generation
        readingFromSave = true
        awaitingCompleteDiagnostics = true
        progress = report
        diagnostics = nil
        review = nil
        readStopped = false
        indexStopped = false
        reviewedLineEnding = nil
        flagBlocks.removeAll()
        tiles.removeAll()
        // Sizing under way reads the old reading, so its result would be
        // dropped: it starts again on the new one.
        if refinedSizingStarted, !isSizingRefined {
            refinedSizingStarted = false
            remeasuring = false
        } else if remeasuring || sizingAfterEdit != nil {
            remeasuring = false
            measureAgainAfterEdit()
        }
        refreshDriveState()
        onChange?(.saved)
        startWaiting()
        progressArrived(report)
    }

    /// A Reload was asked for: the re-readings are off from now, before its
    /// task has even started, until `reloadEnded`.
    func willReload() {
        guard !isReloading else { return }
        isReloading = true
        reloadVersion = editVersion
        onChange?(.progress)
    }

    /// The Reload is over, adopted or not.
    func reloadEnded() {
        guard isReloading else { return }
        isReloading = false
        reloadVersion = nil
        onChange?(.progress)
    }

    /// The user's own choices, for a Reload: kept where they still fit.
    private var reloadOptions: OpenOptions {
        let current = interpretation
        return OpenOptions(
            delimiter: current.delimiterSource == .user ? current.delimiter : nil,
            header: current.headerSource == .user ? current.header : nil,
            encoding: current.encodingSource == .user ? current.encoding : nil,
            firstScreenRows: 1,
            maxChars: GridMetrics.maxCellCharacters
        )
    }

    /// Opens `url` again for a Reload, with the user's choices, or without
    /// them if the chosen encoding no longer fits the file's BOM. Any
    /// thread: it reads the file.
    nonisolated private static func openReloaded(
        url: URL,
        environment: DocumentEnvironment,
        options: OpenOptions,
        reference: ModelReference,
        number: Int
    ) throws -> LealFFI.Document {
        do {
            return try openHandle(url: url, environment: environment, options: options, reference: reference, number: number)
        } catch LealError.EncodingDoesNotFit {
            // The chosen encoding no longer fits the file's BOM.
            let plain = OpenOptions(firstScreenRows: 1, maxChars: GridMetrics.maxCellCharacters)
            return try openHandle(url: url, environment: environment, options: plain, reference: reference, number: number)
        }
    }

    /// Swaps in the core document a Reload opened, as handle `number`.
    /// `recovered`: Recover changes opened it, after a failure, with the
    /// failed document's edits replayed into it (task 2.5.2).
    private func adoptReloaded(_ new: LealFFI.Document, url: URL, reference: ModelReference, number: Int, recovered: Bool = false) throws {
        let screen = try new.firstScreen()
        if recovered {
            // The failed document's state goes: calls work again.
            failure = nil
            observeVolumesAndActivation()
        }

        // From here on, the new core document.
        for task in tasks { task.cancel() }
        tasks.removeAll()
        checkingOriginal?.cancel()
        checkingOriginal = nil
        // A new snapshot: the share's checks start again from nothing.
        shareRecheck?.cancel()
        shareRecheck = nil
        lastDisconnection = nil
        self.url = url
        // The old core document goes off the main thread (app-10).
        CoreRelease.later(&handle)
        handle = new
        handleNumber = number
        reference.model = self
        interpretation = screen.interpretation
        generation = screen.generation
        readingFromSave = false
        awaitingCompleteDiagnostics = false
        fileColumnCount = Int(screen.columnCount)
        progress = IndexProgress(
            generation: screen.generation,
            rows: screen.rowCount,
            estimatedRows: screen.estimatedRowCount,
            bytesScanned: 0,
            bytesTotal: 0,
            complete: false
        )
        reviewedLineEnding = nil
        review = nil
        diagnostics = nil
        flagBlocks.removeAll()
        tiles.removeAll()
        let resized = columnWidths
        columnWidths = []
        widestText = []
        editedWidest.removeAll()
        widestSampleRow = 0
        columnCount = 0
        refinedSizingStarted = false
        remeasuring = false
        isSizingRefined = false
        storage = .clone
        changedOnDisk = false
        readStopped = false
        indexStopped = false
        canSave = true
        original = call({ try $0.original() }) ?? OriginalStatus(state: .unchanged, path: url.path(percentEncoded: false), diverged: false)
        isOnNetworkShare = call({ try $0.isOnNetworkShare() }) ?? isOnNetworkShare
        if recovered {
            // The first screen was read before the replay: the counts and
            // the header row are as the edits leave them now.
            if let current = call({ try $0.progress() }) { progress = current }
            if let count = call({ try $0.columnCount() }) { fileColumnCount = Int(count) }
        }
        applyFirstScreen(screen)
        if recovered, interpretation.header { reloadHeaderTitles() }
        for column in resizedColumns where column < columnWidths.count && column < resized.count {
            columnWidths[column] = resized[column]
        }
        refreshDriveState()
        hasUnsavedEdits = false
        refreshUnsavedEdits()
        if !recovered { onReadingChanged?(.replaced) }
        onChange?(.reloaded)
        startWaiting()
        watchOriginal()
        progressArrived(progress)
    }

    // MARK: Recover changes (task 2.5.2, ADR-0008 decision 5)

    /// **Recover changes** after a failure (DESIGN §3.9): opens the file
    /// afresh, with the journal's `choices` where the file still reads
    /// that way, waits for its index (a row not reached yet would be
    /// refused as `NotReadYet`), replays `commands` into it, and adopts it,
    /// so the window carries on. All of it but the adoption runs off the
    /// main thread (the open reads the file, ADR-0009). Returns the
    /// replay's report, or `nil` if the model closed meanwhile or hasn't
    /// failed.
    ///
    /// - Throws: the open error, if the file can't be opened; the model is
    ///   then still failed.
    func recover(_ commands: [EditCommand], choices: ReadingChoices?) async throws -> ReplayReport? {
        guard failure != nil, handle != nil, !isRecovering else { return nil }
        isRecovering = true
        defer { isRecovering = false }
        let url = url
        let number = handleNumber + 1
        let reference = ModelReference()
        let environment = environment
        let options = reloadOptions
        let opened: LealFFI.Document = try await FileWork.run {
            let opened = try Self.openReloaded(url: url, environment: environment, options: options, reference: reference, number: number)
            // The split the edits were made in, where detection now says
            // otherwise (Treat As or Reopen with Encoding before the
            // edits). Header row or not, the rows are the same.
            if let choices {
                let shown = try opened.firstScreen().interpretation
                if shown.delimiter != choices.delimiter || shown.encoding != choices.encoding {
                    do {
                        _ = try opened.reinterpret(options: OpenOptions(
                            delimiter: choices.delimiter,
                            header: choices.header,
                            encoding: choices.encoding,
                            firstScreenRows: 1,
                            maxChars: GridMetrics.maxCellCharacters
                        ))
                    } catch {
                        // Replayed into another split, every edit would be
                        // refused or land in the wrong cells.
                        throw RecoveryError.readingDoesNotFit(underlying: error)
                    }
                }
            }
            return opened
        }
        var held: LealFFI.Document? = opened
        defer { CoreRelease.later(&held) }
        // A read error stops the index: the rows past it are refused, and
        // named.
        try? await opened.indexJob().finish()
        let report = try await FileWork.run { try opened.replay(commands: commands) }
        guard handle != nil, number == handleNumber + 1 else { return nil }
        try adoptReloaded(opened, url: url, reference: reference, number: number, recovered: true)
        return report
    }

    // MARK: Status bar

    /// What the status bar shows. A failed document shows no rows and isn't
    /// indexing any more (phase 1 review, app-2).
    var status: StatusSummary {
        StatusSummary(
            rows: rowCount,
            columns: failure == nil ? fileColumnCount : 0,
            indexing: failure == nil && !isIndexComplete,
            fractionIndexed: progress.bytesTotal > 0 ? Double(progress.bytesScanned) / Double(progress.bytesTotal) : 0,
            delimiter: interpretation.delimiter,
            lineEnding: reviewedLineEnding ?? interpretation.lineEnding,
            encoding: interpretation.encoding,
            encodingSource: interpretation.encodingSource,
            header: interpretation.header,
            headerSource: interpretation.headerSource,
            readOnly: isReadOnly,
            storage: storage,
            onNetworkShare: isOnNetworkShare,
            changedOnDisk: changedOnDisk,
            readStopped: readStopped,
            original: original.state,
            infoKinds: diagnostics?.diagnostics.filter { $0.severity == .info }.map(\.kind) ?? [],
            warningKinds: Int(diagnostics?.bannerKinds ?? 0),
            notes: interpretation.notes,
            encodingChoices: interpretation.encodingChoices,
            unsavedEdits: failure == nil && hasUnsavedEdits,
            saving: failure == nil ? saveProgress.map(SavingStatus.init) ?? (isWaitingToSave ? SavingStatus(step: .waiting, fraction: nil) : nil) : nil
        )
    }

    /// The whole file's most common line ending, once the review knows it,
    /// else first paint's.
    var lineEnding: LineEnding? { reviewedLineEnding ?? interpretation.lineEnding }
}

/// A Reload, or Save As UTF-8's reading of its copy, wasn't adopted: the
/// document was edited while the file was read, and those edits are kept
/// (`DocumentModel.reloadInBackground`).
struct EditedDuringReload: Error {}

/// Why Recover changes couldn't put the edits back.
enum RecoveryError: Error {
    /// The file no longer reads with the delimiter and encoding the edits
    /// were made in.
    case readingDoesNotFit(underlying: any Error)
}

/// A core document (by `DocumentModel`'s count of Reloads) and a reading
/// of it (its generation).
struct ReadingID: Equatable, Sendable {
    let handle: Int
    let generation: UInt64
}

/// The refined sizing's result, from the background.
private struct RefinedColumns: Sendable {
    /// Each column's widest text.
    let widest: [CGFloat]
    let numeric: [Bool]
    /// The most fields a sampled row had.
    let fieldCount: Int
}

/// The model, for the progress relay, which exists before it. Made on any
/// thread (a file on a network share is opened off the main thread,
/// ADR-0009); the model is set and read only on the main actor.
final class ModelReference: Sendable {
    @MainActor weak var model: DocumentModel?

    nonisolated init() {}
}

extension Logger {
    /// Documents.
    static let document = Logger(subsystem: "io.github.robhaswell.leal", category: "document")
}

/// The core document the grid's reads ahead use, shared with the read
/// queue (task 2.0a review).
final class ReadAheadHandle<Object: AnyObject & Sendable>: @unchecked Sendable {
    // @unchecked: every access holds the lock; the object is itself
    // Sendable.
    private let lock = NSLock()
    private var handle: Object?

    /// Holds `handle` in place of the one before, which is let go of off
    /// the main thread (`CoreRelease`): it may be the last reference to a
    /// closed document.
    func set(_ handle: Object?) {
        var old = lock.withLock {
            let old = self.handle
            self.handle = handle
            return old
        }
        CoreRelease.later(&old)
    }

    func get() -> Object? {
        lock.withLock { handle }
    }
}
