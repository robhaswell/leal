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
    /// Rows in the first screen, from P0 (enough for a tall screen).
    nonisolated static let firstScreenRows: UInt32 = 100
    /// Rows the refined column sizing and number detection read (ADR-0002
    /// question 2; P2 work in DESIGN §3.10).
    nonisolated static let sizingRows: UInt32 = 1_000

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
    /// The rows the column widths were measured from, as (text, truncated),
    /// kept for double-click to fit.
    private var sample: [[(text: String, truncated: Bool)]] = []
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
            options: OpenOptions(firstScreenRows: firstScreenRows, maxChars: GridMetrics.maxCellCharacters),
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

    /// Sizes the columns from the first screen (P0) and asks the core which
    /// are numeric, from the same rows.
    private func applyFirstScreen(_ screen: FirstScreen) {
        var rows = screen.rows.map { $0.map { (text: $0.text, truncated: $0.truncated) } }
        headerTitles = []
        if interpretation.header, !rows.isEmpty {
            headerTitles = rows.removeFirst().map { CellText.display($0.text).text }
        }
        sample = rows
        numeric = call({ try $0.numericColumns(sample: Self.firstScreenRows) }) ?? []
        updateColumnCount()
        columnWidths = measure(rows: rows, columns: columnCount)
    }

    /// The grid's column count: the most common field count, or more if the
    /// header or a row read so far is longer.
    private func updateColumnCount() {
        let widestSample = sample.map(\.count).max() ?? 0
        let count = max(fileColumnCount, headerTitles.count, widestSample, tiles?.widestRow ?? 0)
        guard count != columnCount else { return }
        columnCount = count
        if columnWidths.count < count {
            columnWidths += Array(repeating: GridMetrics.defaultColumnWidth, count: count - columnWidths.count)
        }
    }

    private func measure(rows: [[(text: String, truncated: Bool)]], columns: Int, maximum: CGFloat = GridMetrics.maximumColumnWidth) -> [CGFloat] {
        let header = headerMeasurer
        return ColumnSizer.widths(
            columns: columns,
            header: interpretation.header ? headerTitles : [],
            rows: rows,
            maximum: maximum,
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
    /// rows, off the main thread, once the index has them.
    private func startRefinedSizing() {
        guard let handle, failure == nil else { return }
        refinedSizingStarted = true
        let generation = generation
        let first = UInt64(headerOffset)
        let columns = UInt32(max(columnCount, 1))
        let cell = cellMeasurer
        let number = numberMeasurer
        let header = headerMeasurer
        let titles = interpretation.header ? headerTitles : []
        let task = Task.detached(priority: .utility) { [weak self] in
            // Off the main thread: two reads from the core (each well under
            // a millisecond a hundred rows) and the measuring.
            let result: Result<RefinedColumns, any Error>
            do {
                let rows = try handle.cells(rowStart: first, rowCount: Self.sizingRows, columnStart: 0, columnCount: columns, maxChars: GridMetrics.maxCellCharacters)
                let numeric = try handle.numericColumns(sample: Self.sizingRows)
                let sample = rows.map { $0.cells.map { (text: $0.text, truncated: $0.truncated) } }
                let widths = ColumnSizer.widths(
                    columns: Int(columns),
                    header: titles,
                    rows: sample,
                    measureCell: Self.cellMeasure(numeric: numeric, cell: cell, number: number),
                    measureHeader: { header.width(of: $0) }
                )
                result = .success(RefinedColumns(sample: sample, widths: widths, numeric: numeric))
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
            sample = refined.sample
            numeric = refined.numeric
            for (column, width) in refined.widths.enumerated() where column < columnWidths.count && !resizedColumns.contains(column) {
                columnWidths[column] = width
            }
            updateColumnCount()
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

    /// The width that fits column `column`'s contents in the sample and in
    /// `visibleRows`, for a double-click on its header edge.
    func fittingWidth(column: Int, visibleRows: Range<Int>) -> CGFloat? {
        guard column < columnCount else { return nil }
        var rows = sample.map { row in column < row.count ? [row[column]] : [] }
        for row in visibleRows {
            if case let .text(text, truncated) = cell(row: row, column: column) {
                rows.append([(text, truncated)])
            }
        }
        let title = interpretation.header && column < headerTitles.count ? [headerTitles[column]] : []
        let header = headerMeasurer
        let font = isNumeric(column: column) ? numberMeasurer : cellMeasurer
        return ColumnSizer.widths(
            columns: 1,
            header: title,
            rows: rows,
            maximum: GridMetrics.fitMaximumWidth,
            measureCell: { _, text, truncated in font.cellWidth(of: text, truncated: truncated) },
            measureHeader: { header.width(of: $0) }
        ).first
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
            firstScreenRows: Self.firstScreenRows,
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
            readOnly: isReadOnly
        )
    }
}

/// The refined sizing's result, from the background.
private struct RefinedColumns: Sendable {
    let sample: [[(text: String, truncated: Bool)]]
    let widths: [CGFloat]
    let numeric: [Bool]
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
