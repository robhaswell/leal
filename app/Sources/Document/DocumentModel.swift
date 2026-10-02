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
    /// Opens the core's document in place of `openDocument`. Only tests set
    /// it, to open a file as if on a removable drive that vanishes, or
    /// whose file changes, part-way through (`debugOpenDocumentWithFault`,
    /// task 1.7).
    static var openForTesting: ((_ path: String, _ environment: DocumentEnvironment, _ options: OpenOptions, _ observer: any ProgressObserver) throws -> LealFFI.Document)?
    /// Rows per block of the row-flags cache (gutter markers and hatching).
    nonisolated static let flagBlockRows = 64

    /// Where the file is: where it was opened from, or where it was moved
    /// to since (task 1.9).
    private(set) var url: URL
    private var handle: LealFFI.Document?
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
    /// The index stopped before the end of the file, on a read error (a
    /// drive still there but failing, or the internal disk full while
    /// copying): no more rows will come for this reading, so the window
    /// shows the rows read, says so, and offers Reload (phase 1 review,
    /// app-8).
    private(set) var readStopped = false
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
    private var columnUpdateScheduled = false

    private let cellMeasurer = TextMeasurer(font: GridFonts.cell)
    private let numberMeasurer = TextMeasurer(font: GridFonts.number)
    private let headerMeasurer = TextMeasurer(font: GridFonts.header)

    /// Opens `url` with the core: first paint (P0) happens before this
    /// returns, and the index (P1) and review (P2) start in the background.
    static func open(url: URL, environment: DocumentEnvironment) throws -> DocumentModel {
        let reference = ModelReference()
        // The first screen's rows are read below, through `cells`, with a
        // cap on the fields; the core's first screen need only bring the
        // first row, for the header titles.
        let options = OpenOptions(firstScreenRows: 1, maxChars: GridMetrics.maxCellCharacters)
        let handle = try openHandle(url: url, environment: environment, options: options, reference: reference, number: 0)
        let model = try DocumentModel(url: url, handle: handle, environment: environment)
        reference.model = model
        return model
    }

    /// Opens `url` in the core, with progress for handle `number` relayed
    /// to the model `reference` will point at.
    private static func openHandle(
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
        applyFirstScreen(screen)
        original = call({ try $0.original() }) ?? original
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
        tiles.removeAll()
        stopObserving()
        CoreRelease.later(&handle)
        onChange = nil
        onMoved = nil
    }

    var isFailed: Bool { failure != nil }

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
        } catch let error as LealError {
            switch error {
            case .DocumentFailed:
                fail(error)
            case .DriveDisconnected, .ChangedOnDisk:
                // A read found the drive gone or the file changed: show it
                // (task 1.7). Not now, though: this may be inside a draw.
                queueDriveCheck()
            default:
                Logger.document.error("Core call failed for \(self.url.lastPathComponent, privacy: .public): \(String(describing: error), privacy: .public)")
            }
            return nil
        } catch {
            fail(error)
            return nil
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
        setInteracting(false)
        for task in tasks { task.cancel() }
        tasks.removeAll()
        tiles.removeAll()
        stopObserving()
        onChange?(.failed)
    }

    // MARK: Background jobs

    private func startWaiting() {
        guard let indexJob = call({ try $0.indexJob() }), let reviewJob = call({ try $0.reviewJob() }) else { return }
        let generation = generation
        let number = handleNumber
        tasks.append(Task { [weak self] in
            do {
                try await indexJob.finish()
                self?.indexFinished(generation: generation, handle: number)
            } catch {
                self?.jobEnded(error, job: .index)
            }
        })
        tasks.append(Task { [weak self] in
            do {
                try await reviewJob.finish()
                self?.reviewFinished(generation: generation, handle: number)
            } catch {
                self?.jobEnded(error, job: .review)
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

    /// Background job `job` ended with `error` instead of finishing.
    /// Internal for tests, which end one with a panic or a read error.
    func jobEnded(_ error: any Error, job: BackgroundJob) {
        switch error as? JobFailure {
        case .Panicked?:
            fail(error)
        case .Cancelled?, nil:
            // Closing, re-reading or a cancelled task.
            break
        case .DriveDisconnected?, .ChangedOnDisk?:
            // The drive-disconnected and changed-while-reading banners
            // (ADR-0006, 1.1a). `refreshDriveState` drops rows read before
            // a change (task 1.9); the banner offers Reload.
            Logger.document.error("A background job ended: \(String(describing: error), privacy: .public)")
            if let progress = call({ try $0.progress() }) {
                progressArrived(progress)
            } else {
                refreshDriveState()
                onChange?(.progress)
            }
            if case .DriveDisconnected? = error as? JobFailure {
                // The drive may be back already, before the copy noticed it
                // had gone (a volume that mounts again quickly).
                checkOriginal()
            }
        case .Failed?:
            Logger.document.error("A background job ended: \(String(describing: error), privacy: .public)")
            // Without the review there are only no suggestions. Without the
            // index, no more rows come: stop showing progress, show the rows
            // read, and say so.
            guard job == .index, !readStopped else { return }
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
    }

    /// Reads where the bytes are, whether the file changed while read, and
    /// whether Save is possible (ADR-0006).
    private func refreshDriveState() {
        let changedBefore = changedOnDisk
        storage = call({ try $0.storage() }) ?? storage
        changedOnDisk = call({ try $0.changedOnDisk() }) ?? changedOnDisk
        canSave = call({ try $0.canSave() }) ?? canSave
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
        if readStopped { return loadedRowCount }
        return max(0, Int(progress.estimatedRows) - headerOffset)
    }

    var loadedRowCount: Int {
        guard failure == nil else { return 0 }
        return max(0, Int(progress.rows) - headerOffset)
    }

    /// No more rows will come: the index is complete, or stopped on a read
    /// error. A pending ⌘↓ or Go to Row then goes to the last row read.
    var isIndexComplete: Bool { progress.complete || readStopped }

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
        return read.map { row in
            TileRow(fieldCount: Int(row.fieldCount), cells: row.cells.map { .text($0.text, truncated: $0.truncated) })
        }
    }

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
        tasks.append(task)
    }

    private func applyRefinedSizing(_ result: Result<RefinedColumns, any Error>, reading: ReadingID) {
        guard reading == readingID, failure == nil else { return }
        switch result {
        case let .success(refined):
            numeric = refined.numeric
            widestSampleRow = max(widestSampleRow, refined.fieldCount)
            updateColumnCount()
            for (column, widest) in refined.widest.enumerated() where column < widestText.count {
                widestText[column] = widest
            }
            let widths = ColumnSizer.widths(fromWidest: refined.widest)
            for (column, width) in widths.enumerated() where column < columnWidths.count && !resizedColumns.contains(column) {
                columnWidths[column] = width
            }
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
    /// off and says to Reload first.
    var canReinterpret: Bool { failure == nil && !changedOnDisk }

    /// Reads the file again with the given choices, keeping the user's
    /// earlier ones for the rest. Nothing is reopened (PLAN 1.3).
    private func reinterpret(delimiter: Delimiter? = nil, header: Bool? = nil, encoding: TextEncoding? = nil) {
        guard canReinterpret else { return }
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
        fileColumnCount = Int(screen.columnCount)
        reviewedLineEnding = nil
        review = nil
        diagnostics = nil
        readStopped = false
        flagBlocks.removeAll()
        tiles.removeAll()
        resizedColumns.removeAll()
        columnWidths = []
        widestText = []
        widestSampleRow = 0
        columnCount = 0
        refinedSizingStarted = false
        if let current = call({ try $0.progress() }) { progress = current }
        applyFirstScreen(screen)
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
        let workspace = NSWorkspace.shared.notificationCenter
        let names = [
            (workspace, NSWorkspace.didMountNotification),
            (workspace, NSWorkspace.didUnmountNotification),
            (NotificationCenter.default, NSApplication.didBecomeActiveNotification),
        ]
        for (center, name) in names {
            let observer = center.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { _ = self?.checkOriginal() }
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
            let result = await Task.detached(priority: .utility) { () -> Result<OriginalStatus, any Error> in
                Result { try handle.checkOriginal() }
            }.value
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
        progress = report
        diagnostics = nil
        review = nil
        readStopped = false
        reviewedLineEnding = nil
        flagBlocks.removeAll()
        tiles.removeAll()
        refinedSizingStarted = false
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
        let current = interpretation
        let chosen = OpenOptions(
            delimiter: current.delimiterSource == .user ? current.delimiter : nil,
            header: current.headerSource == .user ? current.header : nil,
            encoding: current.encodingSource == .user ? current.encoding : nil,
            firstScreenRows: 1,
            maxChars: GridMetrics.maxCellCharacters
        )
        let number = handleNumber + 1
        let reference = ModelReference()
        let new: LealFFI.Document
        do {
            new = try Self.openHandle(url: url, environment: environment, options: chosen, reference: reference, number: number)
        } catch LealError.EncodingDoesNotFit {
            // The chosen encoding no longer fits the file's BOM.
            let plain = OpenOptions(firstScreenRows: 1, maxChars: GridMetrics.maxCellCharacters)
            new = try Self.openHandle(url: url, environment: environment, options: plain, reference: reference, number: number)
        }
        let screen = try new.firstScreen()

        // From here on, the new core document.
        for task in tasks { task.cancel() }
        tasks.removeAll()
        checkingOriginal?.cancel()
        checkingOriginal = nil
        self.url = url
        // The old core document goes off the main thread (app-10).
        CoreRelease.later(&handle)
        handle = new
        handleNumber = number
        reference.model = self
        interpretation = screen.interpretation
        generation = screen.generation
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
        widestSampleRow = 0
        columnCount = 0
        refinedSizingStarted = false
        storage = .clone
        changedOnDisk = false
        readStopped = false
        canSave = true
        original = call({ try $0.original() }) ?? OriginalStatus(state: .unchanged, path: url.path(percentEncoded: false), diverged: false)
        applyFirstScreen(screen)
        for column in resizedColumns where column < columnWidths.count && column < resized.count {
            columnWidths[column] = resized[column]
        }
        refreshDriveState()
        onChange?(.reloaded)
        startWaiting()
        watchOriginal()
        progressArrived(progress)
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
            changedOnDisk: changedOnDisk,
            readStopped: readStopped,
            original: original.state,
            infoKinds: diagnostics?.diagnostics.filter { $0.severity == .info }.map(\.kind) ?? [],
            warningKinds: Int(diagnostics?.bannerKinds ?? 0),
            notes: interpretation.notes,
            encodingChoices: interpretation.encodingChoices
        )
    }

    /// The whole file's most common line ending, once the review knows it,
    /// else first paint's.
    var lineEnding: LineEnding? { reviewedLineEnding ?? interpretation.lineEnding }
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

/// The model, for the progress relay, which exists before it.
@MainActor
private final class ModelReference {
    weak var model: DocumentModel?
}

extension Logger {
    /// Documents.
    static let document = Logger(subsystem: "io.github.robhaswell.leal", category: "document")
}
