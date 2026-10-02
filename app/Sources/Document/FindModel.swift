import AppKit
import LealFFI

/// The find bar's state for one document (task 1.8, mockup 04a): the core's
/// `Search` for the query, the current match, and the highlights the grid
/// draws. Matching, counting, Next and Previous all come from the core: this
/// only keeps the answers for the main thread, in grid rows.
///
/// The search is a P2 job in the core (DESIGN §3.10): it streams its
/// matches while it runs (while indexing too), so the count and the
/// highlights are refreshed as it goes, a few times a second, and a Next
/// that the search hasn't reached yet waits for it.
@MainActor
final class FindModel: GridHighlighter {
    /// How often a running search's progress is read.
    nonisolated static let refreshInterval: Duration = .milliseconds(100)
    /// Grid rows and columns of highlights read per core call.
    nonisolated static let tileRows = 64
    nonisolated static let tileColumns = 32

    private let model: DocumentModel

    /// The query, and whether case matters (it doesn't by default, DESIGN
    /// §4.2).
    private(set) var query = ""
    private(set) var caseSensitive = false
    /// The core's search for it.
    private(set) var search: Search?
    private(set) var progress: SearchProgress?
    /// The match that Next or Previous last selected, in grid coordinates,
    /// with its number among the matches.
    private(set) var current: (cell: CellPosition, ordinal: UInt64)?
    /// A Next or Previous the search hadn't got far enough to answer: it is
    /// asked again as the search goes on.
    private(set) var pendingStep: (from: CellPosition?, forward: Bool)?
    /// Next and Previous that found nothing, for tests.
    private(set) var misses = 0
    /// Steps that found a match, and whether the last one went round the
    /// end of the file, and which way: for the "wrapped" sign and
    /// VoiceOver's announcements.
    private(set) var steps = 0
    private(set) var lastStepWrapped = false
    private(set) var lastStepForward = true

    /// Something the find bar or the grid shows changed.
    var onChange: (() -> Void)?
    /// A match should be selected and shown.
    var onSelect: ((CellPosition) -> Void)?

    private var tiles: [TileKey: HighlightTile] = [:]
    private var tasks: [Task<Void, Never>] = []

    private struct TileKey: Hashable {
        let rowBlock: Int
        let columnBlock: Int
    }

    private struct HighlightTile {
        var cells: [CellPosition: [NSRange]]
        /// Rows searched when it was read: it is read again once the search
        /// has gone past it.
        let searchedAtRead: UInt64
        /// Every row of it was searched when it was read.
        let settled: Bool
    }

    init(model: DocumentModel) {
        self.model = model
    }

    var isSearching: Bool { search != nil && progress?.complete != true }
    var matchCount: UInt64 { progress?.matches ?? 0 }

    // MARK: Searching

    /// Searches for `text` (an empty one clears the search), and selects the
    /// first match at or after `from`, the active cell, once it is found:
    /// typing in the find bar shows the next match as it goes, as macOS
    /// find bars do.
    func find(_ text: String, caseSensitive: Bool, from: CellPosition?) {
        stop()
        query = text
        self.caseSensitive = caseSensitive
        current = nil
        guard !text.isEmpty else {
            onChange?()
            return
        }
        // `nil` for a query the core won't search for (too long).
        let found: Search?? = model.call { handle -> Search? in
            try handle.find(text: text, caseSensitive: caseSensitive)
        }
        guard let started = found ?? nil else {
            onChange?()
            return
        }
        search = started
        progress = model.call { _ in try started.progress() }
        // The first match at or after the active cell: Next from the cell
        // before it.
        pendingStep = (from.flatMap { cellBefore($0) }, true)
        watch(started)
        retryPendingStep()
        onChange?()
    }

    /// The query again, if the file was read again (its rows changed).
    func restart(from: CellPosition?) {
        guard !query.isEmpty else { return }
        find(query, caseSensitive: caseSensitive, from: from)
    }

    /// Stops the search and forgets its matches (the find bar closed).
    func stop() {
        for task in tasks { task.cancel() }
        tasks.removeAll()
        search?.cancel()
        // Off the main thread: it may hold the last reference to a core
        // document (phase 1 review, app-10; `CoreRelease`).
        CoreRelease.later(&search)
        progress = nil
        pendingStep = nil
        current = nil
        tiles.removeAll()
    }

    /// Waits for the search in the background: its job (through
    /// `Job.finish()`, so cancelling the task stops it) and, meanwhile, its
    /// progress a few times a second.
    ///
    /// Neither task holds the search itself, only its job, which `stop()`
    /// cancels: so closing the window or a new query lets the search, and
    /// the core document it reads, go at once (1.8 review).
    private func watch(_ search: Search) {
        let job = search.job()
        let id = ObjectIdentifier(search)
        tasks.append(Task { [weak self] in
            try? await job.finish()
            self?.refresh(id)
        })
        tasks.append(Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.refreshInterval)
                guard let self, let current = self.search, ObjectIdentifier(current) == id else { return }
                refresh(id)
                if progress?.complete == true { return }
            }
        })
    }

    /// Reads the search's progress; if it moved on, the highlights and a
    /// waiting Next are brought up to date.
    private func refresh(_ id: ObjectIdentifier) {
        guard let search, ObjectIdentifier(search) == id else { return }
        guard let latest = model.call({ _ in try search.progress() }) else { return }
        guard latest != progress else { return }
        progress = latest
        retryPendingStep()
        onChange?()
    }

    // MARK: Next and Previous

    /// ⌘G (`forward`) or ⇧⌘G from `from`, the active cell: the next or
    /// previous match, round the end of the file once the search is
    /// complete. If the search hasn't got that far, it waits for it.
    func step(forward: Bool, from: CellPosition?) {
        guard search != nil else { return }
        pendingStep = (from, forward)
        retryPendingStep()
    }

    /// The user went somewhere themselves (a click, a key, a scroll, Go to
    /// Row): a Next still waiting for the search, or the first match typing
    /// would select, mustn't take the selection away from them.
    func cancelPendingStep() {
        pendingStep = nil
    }

    private func retryPendingStep() {
        guard let search, let (from, forward) = pendingStep else { return }
        let headerRows = model.headerRows
        let physical = from.map { (row: UInt64($0.row + headerRows), column: UInt32(clamping: $0.column)) }
        guard let step = model.call({ _ in
            try search.step(row: physical?.row, column: physical?.column ?? 0, forward: forward)
        }) else {
            pendingStep = nil
            return
        }
        switch step {
        case let .found(row, column, ordinal, wrapped):
            pendingStep = nil
            steps += 1
            lastStepWrapped = wrapped
            lastStepForward = forward
            let cell = CellPosition(row: model.gridRow(ofPhysical: row), column: Int(column))
            current = (cell, ordinal)
            onSelect?(cell)
            onChange?()
        case .pending:
            break
        case .notFound:
            pendingStep = nil
            misses += 1
            onChange?()
        }
    }

    /// The selection moved to `cell` (a click, a key): it is the current
    /// match if it is one, so the bar says "k of N".
    func activeCellChanged(_ cell: CellPosition?) {
        guard let search, let cell else {
            if current != nil { current = nil; onChange?() }
            return
        }
        if current?.cell == cell { return }
        let row = UInt64(cell.row + model.headerRows)
        let ordinal = model.call { _ in try search.ordinal(row: row, column: UInt32(clamping: cell.column)) } ?? nil
        let wasCurrent = current != nil
        current = ordinal.map { (cell, $0) }
        if wasCurrent || current != nil { onChange?() }
    }

    /// The cell before `cell` in file order, so a Next from it finds `cell`
    /// itself if it is a match. Before the first cell of the first data
    /// row is the header row's last cell (a header row is never searched),
    /// or nothing.
    private func cellBefore(_ cell: CellPosition) -> CellPosition? {
        if cell.column > 0 { return CellPosition(row: cell.row, column: cell.column - 1) }
        if cell.row + model.headerRows > 0 { return CellPosition(row: cell.row - 1, column: Int(UInt32.max)) }
        return nil
    }

    // MARK: Highlights (GridHighlighter)

    func prepareHighlights(rows: Range<Int>, columns: Range<Int>) {
        guard search != nil, !rows.isEmpty, !columns.isEmpty else { return }
        for rowBlock in (rows.lowerBound / Self.tileRows)...((rows.upperBound - 1) / Self.tileRows) {
            for columnBlock in (columns.lowerBound / Self.tileColumns)...((columns.upperBound - 1) / Self.tileColumns) {
                _ = tile(TileKey(rowBlock: rowBlock, columnBlock: columnBlock))
            }
        }
    }

    func highlight(row: Int, column: Int) -> CellHighlight? {
        guard search != nil, row >= 0, column >= 0 else { return nil }
        let key = TileKey(rowBlock: row / Self.tileRows, columnBlock: column / Self.tileColumns)
        let cell = CellPosition(row: row, column: column)
        guard let ranges = tile(key)?.cells[cell] else { return nil }
        return CellHighlight(ranges: ranges, isCurrent: current?.cell == cell)
    }

    /// A tile of highlights, read from the core if it isn't yet or the
    /// search has gone past it since.
    private func tile(_ key: TileKey) -> HighlightTile? {
        guard let search else { return nil }
        let searched = progress?.rowsSearched ?? 0
        if let tile = tiles[key], tile.settled || tile.searchedAtRead == searched {
            return tile
        }
        if tiles.count > 64 { tiles.removeAll() }
        let headerRows = model.headerRows
        let firstRow = key.rowBlock * Self.tileRows
        let firstColumn = key.columnBlock * Self.tileColumns
        let start = UInt64(firstRow + headerRows)
        let matches = model.call { _ in
            try search.matchesIn(
                rowStart: start,
                rowCount: UInt32(Self.tileRows),
                columnStart: UInt32(firstColumn),
                columnCount: UInt32(Self.tileColumns),
                maxChars: GridMetrics.maxCellCharacters
            )
        } ?? []
        var cells: [CellPosition: [NSRange]] = [:]
        for found in matches {
            let cell = CellPosition(row: model.gridRow(ofPhysical: found.row), column: Int(found.column))
            cells[cell] = found.ranges.map { NSRange(location: Int($0.start), length: Int($0.length)) }
        }
        let end = start + UInt64(Self.tileRows)
        let tile = HighlightTile(cells: cells, searchedAtRead: searched, settled: progress?.complete == true || end <= searched)
        tiles[key] = tile
        return tile
    }
}
