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

    /// Called after first paint inside `init`, on the core's document. Only
    /// tests set it, to make the document fail while it opens.
    static var afterFirstPaintForTesting: ((LealFFI.Document) throws -> Void)?

    let url: URL
    private var handle: LealFFI.Document?
    private let scheduler: Scheduler

    /// Called on the main actor when something the window shows changed.
    var onChange: ((DocumentChange) -> Void)?

    private(set) var interpretation: Interpretation
    private(set) var generation: UInt64
    private(set) var progress: IndexProgress
    /// The whole file's line ending, once the review (P2) has finished.
    private(set) var reviewedLineEnding: LineEnding?
    /// The most common field count: the "× N columns" of the status bar.
    private(set) var fileColumnCount: Int
    /// Where the file's bytes are held (DESIGN §3.1): a clone, or a copy in
    /// memory when the volume can't clone, which the status bar notes.
    private(set) var storage: SourceStorage = .clone
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
        let relay = ProgressRelay { progress in reference.model?.progressArrived(progress) }
        let handle = try openDocument(
            path: url.path(percentEncoded: false),
            volume: TemporaryFolders.volume(for: url),
            temp: environment.temp,
            scheduler: environment.scheduler,
            // The first screen's rows are read below, through `cells`, with
            // a cap on the fields; the core's first screen need only bring
            // the first row, for the header titles.
            options: OpenOptions(firstScreenRows: 1, maxChars: GridMetrics.maxCellCharacters),
            observer: relay
        )
        let model = try DocumentModel(url: url, handle: handle, scheduler: environment.scheduler)
        reference.model = model
        return model
    }

    private init(url: URL, handle: LealFFI.Document, scheduler: Scheduler) throws {
        self.url = url
        self.handle = handle
        self.scheduler = scheduler
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
        storage = call({ try $0.storage() }) ?? storage
        if let hook = Self.afterFirstPaintForTesting {
            _ = call { try hook($0) }
        }
    }

    /// Starts waiting for the background jobs. Call once the window exists.
    func start() {
        guard failure == nil, tasks.isEmpty else { return }
        startWaiting()
        progressArrived(progress)
    }

    /// Stops everything: the Swift tasks are cancelled, which cancels the
    /// jobs they wait for, and the core's document is released, which
    /// cancels the rest and deletes the file's clone (DESIGN §3.1).
    func close() {
        setInteracting(false)
        for task in tasks { task.cancel() }
        tasks.removeAll()
        tiles.removeAll()
        handle = nil
        onChange = nil
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
            if case .DocumentFailed = error {
                fail(error)
            } else {
                Logger.document.error("Core call failed for \(self.url.lastPathComponent, privacy: .public): \(String(describing: error), privacy: .public)")
            }
            return nil
        } catch {
            fail(error)
            return nil
        }
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
        onChange?(.failed)
    }

    // MARK: Background jobs

    private func startWaiting() {
        guard let indexJob = call({ try $0.indexJob() }), let reviewJob = call({ try $0.reviewJob() }) else { return }
        let generation = generation
        tasks.append(Task { [weak self] in
            do {
                try await indexJob.finish()
                self?.indexFinished(generation: generation)
            } catch {
                self?.jobEnded(error)
            }
        })
        tasks.append(Task { [weak self] in
            do {
                try await reviewJob.finish()
                self?.reviewFinished(generation: generation)
            } catch {
                self?.jobEnded(error)
            }
        })
    }

    private func jobEnded(_ error: any Error) {
        switch error as? JobFailure {
        case .Panicked?:
            fail(error)
        case .Cancelled?, nil:
            // Closing, re-reading or a cancelled task.
            break
        case .DriveDisconnected?, .ChangedOnDisk?, .Failed?:
            // SEAM(1.7, 1.9): the drive-disconnected and changed-elsewhere
            // banners.
            Logger.document.error("A background job ended: \(String(describing: error), privacy: .public)")
        }
    }

    private func indexFinished(generation: UInt64) {
        guard generation == self.generation, let progress = call({ try $0.progress() }) else { return }
        progressArrived(progress)
    }

    private func reviewFinished(generation: UInt64) {
        guard generation == self.generation, let review = call({ try $0.review() }) else { return }
        reviewedLineEnding = review?.lineEnding ?? reviewedLineEnding
        // SEAM(1.7): the encoding and delimiter suggestion banners.
        onChange?(.progress)
    }

    /// A progress report from the relay (or a fresh one).
    private func progressArrived(_ report: IndexProgress) {
        guard failure == nil, report.generation == generation else { return }
        progress = report
        if let count = call({ try $0.columnCount() }) {
            fileColumnCount = Int(count)
        }
        // A removable drive's file moves from being read to a copy as the
        // index pass copies it. SEAM(1.7): Copy, Reading and Disconnected.
        storage = call({ try $0.storage() }) ?? storage
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
        return max(0, Int(progress.estimatedRows) - headerOffset)
    }

    var loadedRowCount: Int {
        guard failure == nil else { return 0 }
        return max(0, Int(progress.rows) - headerOffset)
    }

    var isIndexComplete: Bool { progress.complete }

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
        let generation = generation
        let first = UInt64(headerOffset)
        let columns = max(columnCount, 1)
        let rowCount = Self.sizingRows(wanted: Self.sizingRows, columns: columns)
        largestSizingRead = max(largestSizingRead, Int(rowCount) * columns)
        let cell = cellMeasurer
        let number = numberMeasurer
        let header = headerMeasurer
        let titles = interpretation.header ? headerTitles : []
        let task = Task.detached(priority: .utility) { [weak self] in
            // Off the main thread: two reads from the core (each well under
            // a millisecond a hundred rows) and the measuring. Only each
            // column's widest text is kept.
            let result: Result<RefinedColumns, any Error>
            do {
                let rows = try handle.cells(
                    rowStart: first,
                    rowCount: rowCount,
                    columnStart: 0,
                    columnCount: UInt32(columns),
                    maxChars: GridMetrics.maxCellCharacters
                )
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
            await self?.applyRefinedSizing(result, generation: generation)
        }
        tasks.append(task)
    }

    private func applyRefinedSizing(_ result: Result<RefinedColumns, any Error>, generation: UInt64) {
        guard generation == self.generation, failure == nil else { return }
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
        let current = interpretation
        let options = OpenOptions(
            delimiter: current.delimiterSource == .user ? current.delimiter : nil,
            header: header,
            encoding: current.encodingSource == .user ? current.encoding : nil,
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

    // MARK: Status bar

    var status: StatusSummary {
        StatusSummary(
            rows: max(0, Int(progress.estimatedRows) - headerOffset),
            columns: fileColumnCount,
            indexing: !progress.complete,
            fractionIndexed: progress.bytesTotal > 0 ? Double(progress.bytesScanned) / Double(progress.bytesTotal) : 0,
            delimiter: interpretation.delimiter,
            lineEnding: reviewedLineEnding ?? interpretation.lineEnding,
            encoding: interpretation.encoding,
            encodingSource: interpretation.encodingSource,
            header: interpretation.header,
            headerSource: interpretation.headerSource,
            readOnly: isReadOnly,
            storage: storage
        )
    }
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
