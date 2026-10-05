import Foundation

/// What the grid shows in one cell.
enum GridCell: Equatable, Sendable {
    /// A value, perhaps empty. `truncated` says the core gave only its
    /// start (`GridMetrics.maxCellCharacters`).
    case text(String, truncated: Bool)
    /// The row has fewer fields than this column has (a short row). Drawn
    /// hatched when the core says the row is ragged
    /// (`GridDataSource.isHatched`).
    case missing
    /// The row hasn't been read yet: it is past the indexed region. The
    /// grid draws a skeleton (mockup 02b).
    case notLoaded
}

/// How a header title is drawn.
enum HeaderStyle: Equatable, Sendable {
    /// A name from the file's header row.
    case name
    /// No header row: the column's number, in grey (ADR-0002 question 13).
    case number
    /// A column past the header row's last field, which only long rows
    /// have: "Column N", dimmed (ADR-0002 question 5).
    case extra
}

struct HeaderTitle: Equatable, Sendable {
    let text: String
    let style: HeaderStyle
}

/// Where the grid gets what it draws. `DocumentModel` reads it from the
/// core; tests use small fakes. Rows here are the grid's rows: the header
/// row, if the file has one, is not one of them.
@MainActor
protocol GridDataSource: AnyObject {
    /// Rows to lay out: the estimated row count while indexing (DESIGN
    /// §3.10 rule 5), exact once indexing is complete.
    var rowCount: Int { get }
    /// Rows that can be read now. Rows from here to `rowCount` are drawn
    /// as skeletons.
    var loadedRowCount: Int { get }
    var columnCount: Int { get }
    func headerTitle(column: Int) -> HeaderTitle
    /// Whether the column holds numbers, which are right-aligned.
    func isNumeric(column: Int) -> Bool
    func cell(row: Int, column: Int) -> GridCell
    /// Called before a region is drawn, so the source can read it in one go.
    func prepare(rows: Range<Int>, columns: Range<Int>)
    /// Whether the row's gutter has a diagnostics marker (ADR-0002
    /// question 7).
    func rowHasMarker(_ row: Int) -> Bool
    /// Whether a `.missing` cell is drawn hatched: the row is ragged
    /// (ADR-0002 question 5).
    func isHatched(row: Int, column: Int) -> Bool
    /// Whether the cell holds an unsaved edit, drawn with a corner
    /// triangle (mockup 05a, task 2.5.2). Asked after `cell(row:column:)`.
    func isEdited(row: Int, column: Int) -> Bool
    /// The cell, if it is already read; `nil` if it would need a read from
    /// the core now. For laying out text ahead of the scroll (task 2.0a).
    func cachedCell(row: Int, column: Int) -> GridCell?
    /// Reads a region off the main thread, ahead of drawing it (task 2.0a).
    func readAhead(rows: Range<Int>, columns: Range<Int>)
}

extension GridDataSource {
    func rowHasMarker(_ row: Int) -> Bool { false }
    func isHatched(row: Int, column: Int) -> Bool { false }
    func isEdited(row: Int, column: Int) -> Bool { false }
    func cachedCell(row: Int, column: Int) -> GridCell? { cell(row: row, column: column) }
    func readAhead(rows: Range<Int>, columns: Range<Int>) {}
}

/// One row of a tile: the row's whole field count, and its cells in the
/// tile's columns.
struct TileRow: Equatable, Sendable {
    let fieldCount: Int
    let cells: [GridCell]
    /// The columns whose cells hold an edit (the core's `edited`), in
    /// order; empty for a row with none.
    var edited: [Int] = []
}

/// Cells read from the core in tiles of 64 rows × 32 columns, with the
/// least recently used tiles dropped beyond `capacity`. One FFI call reads
/// a tile, so scrolling costs one call every few frames, and a wide file
/// only reads the columns on screen.
///
/// A tile read while indexing may stop short at the last indexed row; it
/// is read again once more rows can be.
@MainActor
final class CellTileCache {
    static let rowsPerTile = 64
    static let columnsPerTile = 32

    /// Reads rows and columns; `nil` if they can't be read (the tile is
    /// then left empty until `removeAll`).
    typealias Fetch = (_ rows: Range<Int>, _ columns: Range<Int>) -> [TileRow]?

    private struct Key: Hashable {
        let rowBlock: Int
        let columnBlock: Int
    }

    private struct Tile {
        var rows: [TileRow]
        /// How many rows were asked for.
        let requested: Int
        /// The read failed; don't read it again.
        let failed: Bool
        var lastUse: UInt64
    }

    /// Reads rows and columns off the main thread. Made for each read
    /// (`makeBackgroundFetch`), from the document as it is when the read is
    /// asked for. It throws what the core threw; `nil` means it had nothing
    /// to read from (the document closed).
    typealias BackgroundFetch = @Sendable (_ rows: Range<Int>, _ columns: Range<Int>) throws -> [TileRow]?

    /// The most tiles a call to `readAhead` starts reading, and the most
    /// being read at once: a jump across the file (a scroller drag, Go to
    /// Row) asks for a region only a screen or two tall, but nothing here
    /// relies on that.
    nonisolated static let newReadsPerCall = 4
    nonisolated static let readsInFlight = 8

    private let minimumCapacity: Int
    private var capacity: Int
    private let fetch: Fetch
    /// Gives a reader for tiles read ahead of the scroll (task 2.0a), or
    /// `nil` if none can be read now.
    var makeBackgroundFetch: (() -> BackgroundFetch?)?
    /// A read ahead failed: the document decides what the error means, as
    /// for any core call (`DocumentModel.report`).
    var onReadAheadError: ((any Error) -> Void)?
    private var tiles: [Key: Tile] = [:]
    private var clock: UInt64 = 0
    /// Tiles being read ahead, each with the token of its read: an answer
    /// that comes back with another token is for a read the cache has
    /// since dropped (`removeAll`, `invalidate`), and is thrown away.
    private var reading: [Key: UInt64] = [:]
    private var nextToken: UInt64 = 0
    /// Tiles whose read ahead failed since the cache was last emptied: not
    /// asked for again ahead (a vanished drive would fail every frame).
    /// The draw still reads one it needs, and reports the error then.
    private var failedAhead: Set<Key> = []
    /// The rows wanted since the last frame (the first `prepare` of a
    /// frame starts it again, later calls widen it), which a queued read
    /// checks before it starts: one the scroll has left behind is skipped.
    private let wanted = WantedRange()
    /// A frame's first `prepare` has started `wanted` again, and the
    /// run-loop turn it is drawn in is not over: the strips of a full
    /// redraw are drawn one after another in it, and each widens the
    /// range instead of resetting it, so the reads ahead of the first
    /// are not skipped.
    private var frameStarted = false
    /// The most fields any row read so far has had.
    private(set) var widestRow = 0
    /// How many reads there have been, for tests.
    private(set) var fetchCount = 0
    /// How many tiles were read ahead, and how many reads ahead were
    /// started, for tests.
    private(set) var readAheadCount = 0
    private(set) var readsAheadStarted = 0
    /// How many reads ahead have come back, kept or not, for tests.
    private(set) var readsAheadBack = 0
    /// How many reads ahead are under way.
    var readsAheadUnderWay: Int { reading.count }

    init(capacity: Int = 24, fetch: @escaping Fetch) {
        minimumCapacity = max(1, capacity)
        self.capacity = minimumCapacity
        self.fetch = fetch
    }

    /// Reads ahead, off the main thread, the tiles of a region that aren't
    /// read or being read yet (task 2.0a): at most `newReadsPerCall` new
    /// ones, nearest the region's start first, and never more than
    /// `readsInFlight` at once. A tile read from the core costs about a
    /// millisecond on the main thread, most of it turning the core's reply
    /// into Swift values, and scrolling meets a new tile every few frames.
    /// A tile still missing when it is drawn is read there and then, as
    /// before, so nothing waits for these.
    func readAhead(rows: Range<Int>, columns: Range<Int>, loadedRows: Int, startsWanted: Bool = false) {
        let rows = rows.clamped(to: 0..<max(0, loadedRows))
        guard !rows.isEmpty else { return }
        let firstRowBlock = rows.lowerBound / Self.rowsPerTile
        let lastRowBlock = (rows.upperBound - 1) / Self.rowsPerTile
        // Before any early return: the frame's range starts again (or
        // widens) even when there is nothing to read ahead, so a read
        // queued earlier is checked against this frame's rows, not an
        // earlier one's.
        if startsWanted {
            wanted.set((firstRowBlock - 1)...(lastRowBlock + 1))
        } else {
            wanted.widen((firstRowBlock - 1)...(lastRowBlock + 1))
        }
        guard makeBackgroundFetch != nil, !columns.isEmpty else { return }
        // Column blocks past every row read so far have nothing to read.
        let columnBlocks = max(1, (widestRow + Self.columnsPerTile - 1) / Self.columnsPerTile)
        let firstColumnBlock = max(0, columns.lowerBound) / Self.columnsPerTile
        let lastColumnBlock = min(columnBlocks - 1, (columns.upperBound - 1) / Self.columnsPerTile)
        guard firstColumnBlock <= lastColumnBlock else { return }
        var started = 0
        var rowBlock = firstRowBlock
        // Lazily, block by block: the region may be long, the reads few.
        while rowBlock <= lastRowBlock, started < Self.newReadsPerCall, reading.count < Self.readsInFlight {
            for columnBlock in firstColumnBlock...lastColumnBlock
                where started < Self.newReadsPerCall && reading.count < Self.readsInFlight
            {
                let key = Key(rowBlock: rowBlock, columnBlock: columnBlock)
                guard tiles[key] == nil, reading[key] == nil, !failedAhead.contains(key), let read = makeBackgroundFetch?() else { continue }
                nextToken += 1
                let token = nextToken
                reading[key] = token
                started += 1
                readsAheadStarted += 1
                let tileRows = (rowBlock * Self.rowsPerTile)..<((rowBlock + 1) * Self.rowsPerTile)
                let tileColumns = (columnBlock * Self.columnsPerTile)..<((columnBlock + 1) * Self.columnsPerTile)
                let wanted = wanted
                let block = rowBlock
                Self.readQueue.async { [weak self] in
                    // The scroll may have moved on while this waited.
                    guard wanted.contains(block) else {
                        Task { @MainActor [weak self] in self?.arrived(key, token: token, .success(nil)) }
                        return
                    }
                    let result = Result { try read(tileRows, tileColumns) }
                    Task { @MainActor [weak self] in self?.arrived(key, token: token, result) }
                }
            }
            rowBlock += 1
        }
    }

    /// The cell at `row`, `column` if its tile is read; `nil` if it isn't.
    func cachedCell(row: Int, column: Int, loadedRows: Int) -> GridCell? {
        guard row >= 0, column >= 0, row < loadedRows else { return .notLoaded }
        let key = Key(rowBlock: row / Self.rowsPerTile, columnBlock: column / Self.columnsPerTile)
        guard let tile = tiles[key] else { return nil }
        let offset = row - key.rowBlock * Self.rowsPerTile
        guard offset < tile.rows.count else { return tile.failed ? .notLoaded : nil }
        let tileRow = tile.rows[offset]
        if column >= tileRow.fieldCount { return .missing }
        let cellIndex = column - key.columnBlock * Self.columnsPerTile
        return cellIndex < tileRow.cells.count ? tileRow.cells[cellIndex] : .missing
    }

    /// Whether the cell, read already, holds an edit (the core names them in
    /// each row it reads). `false` for a cell not read yet.
    func isEdited(row: Int, column: Int) -> Bool {
        guard row >= 0, column >= 0 else { return false }
        let key = Key(rowBlock: row / Self.rowsPerTile, columnBlock: column / Self.columnsPerTile)
        guard let tile = tiles[key] else { return false }
        let offset = row - key.rowBlock * Self.rowsPerTile
        guard offset < tile.rows.count else { return false }
        let edited = tile.rows[offset].edited
        return !edited.isEmpty && edited.contains(column)
    }

    /// The reads ahead, one at a time, for every document: each holds the
    /// core's row cache while it reads (the grid's own reads on the main
    /// thread wait for it), and reads only bytes already in memory or in
    /// the file's copy, never a share (task 2.0), so none can stall.
    private nonisolated static let readQueue = DispatchQueue(label: "io.github.robhaswell.leal.read-ahead", qos: .userInitiated)

    /// Waits for the reads ahead already queued, so that none still holds
    /// a document when Leal quits (`CoreRelease.finish`).
    nonisolated static func finishReadsAhead(timeout: DispatchTime = .distantFuture) {
        CoreRelease.wait(for: readQueue, timeout: timeout)
    }

    /// Holds the reads ahead queued from now on until `resumeReadsAhead`,
    /// for tests of what happens while one is under way. Resuming more
    /// than once is harmless, so a test can resume in a `defer` too.
    nonisolated static func suspendReadsAhead() {
        readQueueHold.hold(readQueue)
    }

    nonisolated static func resumeReadsAhead() {
        readQueueHold.release(readQueue)
    }

    private nonisolated static let readQueueHold = QueueHold()

    private func arrived(_ key: Key, token: UInt64, _ result: Result<[TileRow]?, any Error>) {
        readsAheadBack += 1
        // A read the cache dropped since (or one whose key was asked for
        // again) leaves the newer read alone.
        guard reading[key] == token else { return }
        reading[key] = nil
        switch result {
        case let .success(rows?):
            guard tiles[key] == nil else { return }
            clock += 1
            readAheadCount += 1
            tiles[key] = Tile(rows: rows, requested: Self.rowsPerTile, failed: false, lastUse: clock)
            for row in rows where row.fieldCount > widestRow {
                widestRow = row.fieldCount
            }
            evictIfNeeded()
        case .success(nil):
            break
        case let .failure(error):
            failedAhead.insert(key)
            onReadAheadError?(error)
        }
    }

    /// The cell at `row`, `column`. `loadedRows` is how many rows can be
    /// read now.
    func cell(row: Int, column: Int, loadedRows: Int) -> GridCell {
        guard row >= 0, column >= 0, row < loadedRows else { return .notLoaded }
        let key = Key(rowBlock: row / Self.rowsPerTile, columnBlock: column / Self.columnsPerTile)
        guard let tile = tile(for: key, loadedRows: loadedRows) else { return .notLoaded }
        let offset = row - key.rowBlock * Self.rowsPerTile
        guard offset < tile.rows.count else { return .notLoaded }
        let tileRow = tile.rows[offset]
        if column >= tileRow.fieldCount { return .missing }
        let cellIndex = column - key.columnBlock * Self.columnsPerTile
        return cellIndex < tileRow.cells.count ? tileRow.cells[cellIndex] : .missing
    }

    /// Reads every tile of a region that isn't read yet.
    func prepare(rows: Range<Int>, columns: Range<Int>, loadedRows: Int) {
        let rows = rows.clamped(to: 0..<max(0, loadedRows))
        guard !rows.isEmpty, !columns.isEmpty, columns.lowerBound >= 0 else { return }
        makeRoom(rows: rows, columns: columns)
        for rowBlock in (rows.lowerBound / Self.rowsPerTile)...((rows.upperBound - 1) / Self.rowsPerTile) {
            for columnBlock in (columns.lowerBound / Self.columnsPerTile)...((columns.upperBound - 1) / Self.columnsPerTile) {
                _ = tile(for: Key(rowBlock: rowBlock, columnBlock: columnBlock), loadedRows: loadedRows)
            }
        }
        // The tiles next to it, above and below, and either side. A frame
        // starts what is wanted again (once, however many strips it draws):
        // a read queued for rows the scroll has left is skipped.
        let block = Self.rowsPerTile
        let startsFrame = !frameStarted
        if startsFrame {
            frameStarted = true
            DispatchQueue.main.async { [weak self] in
                MainActor.assumeIsolated { self?.frameStarted = false }
            }
        }
        readAhead(rows: max(0, rows.lowerBound - block)..<(rows.upperBound + block), columns: columns, loadedRows: loadedRows, startsWanted: startsFrame)
        readAhead(rows: rows, columns: max(0, columns.lowerBound - Self.columnsPerTile)..<(columns.upperBound + Self.columnsPerTile), loadedRows: loadedRows)
    }

    /// Keeps room for the tiles of a region drawn and those around it, which
    /// are read ahead (task 2.0a review): four times the region's, however
    /// wide the window.
    private func makeRoom(rows: Range<Int>, columns: Range<Int>) {
        let rowBlocks = min((rows.upperBound - 1) / Self.rowsPerTile - rows.lowerBound / Self.rowsPerTile, 3) + 1
        let columnBlocks = (columns.upperBound - 1) / Self.columnsPerTile - columns.lowerBound / Self.columnsPerTile + 1
        capacity = max(capacity, minimumCapacity, 4 * rowBlocks * columnBlocks + 8)
    }

    func removeAll() {
        tiles.removeAll()
        reading.removeAll()
        failedAhead.removeAll()
        widestRow = 0
    }

    /// Forgets the tiles holding `rows`, and any read of them under way,
    /// whose answer is then thrown away: their cells changed (an edit,
    /// `DocumentModel.cellsChanged`). They are read again when drawn.
    /// Rows inserted or deleted move every row after them: use
    /// `removeAll` for those.
    func invalidate(rows: Range<Int>) {
        guard !rows.isEmpty else { return }
        let blocks = (rows.lowerBound / Self.rowsPerTile)...((rows.upperBound - 1) / Self.rowsPerTile)
        for key in Array(tiles.keys) where blocks.contains(key.rowBlock) {
            tiles[key] = nil
        }
        for key in Array(reading.keys) where blocks.contains(key.rowBlock) {
            reading[key] = nil
        }
        failedAhead = failedAhead.filter { !blocks.contains($0.rowBlock) }
    }

    private func tile(for key: Key, loadedRows: Int) -> Tile? {
        clock += 1
        let start = key.rowBlock * Self.rowsPerTile
        if var tile = tiles[key] {
            let short = tile.rows.count < tile.requested && start + tile.rows.count < loadedRows
            if !short || tile.failed {
                tile.lastUse = clock
                tiles[key] = tile
                return tile
            }
        }
        let rows = start..<(start + Self.rowsPerTile)
        let columnStart = key.columnBlock * Self.columnsPerTile
        let columns = columnStart..<(columnStart + Self.columnsPerTile)
        fetchCount += 1
        let read = fetch(rows, columns)
        let tile = Tile(rows: read ?? [], requested: Self.rowsPerTile, failed: read == nil, lastUse: clock)
        for row in tile.rows where row.fieldCount > widestRow {
            widestRow = row.fieldCount
        }
        tiles[key] = tile
        evictIfNeeded()
        return tile
    }

    private func evictIfNeeded() {
        guard tiles.count > capacity else { return }
        // Drop the least recently used quarter in one go, so this sort
        // happens rarely.
        let excess = tiles.count - capacity + capacity / 4
        let oldest = tiles.sorted { $0.value.lastUse < $1.value.lastUse }.prefix(excess)
        for (key, _) in oldest {
            tiles.removeValue(forKey: key)
        }
    }
}

/// What reads or layout ahead are still wanted for (row blocks, rows),
/// shared with the queued work, which skips what the scroll has left (task
/// 2.0a review).
final class WantedRange: @unchecked Sendable {
    // @unchecked: every access holds the lock.
    private let lock = NSLock()
    private var range: ClosedRange<Int> = 0...0

    func set(_ range: ClosedRange<Int>) {
        lock.withLock { self.range = range }
    }

    func widen(_ range: ClosedRange<Int>) {
        lock.withLock {
            self.range = min(self.range.lowerBound, range.lowerBound)...max(self.range.upperBound, range.upperBound)
        }
    }

    func contains(_ value: Int) -> Bool {
        lock.withLock { range.contains(value) }
    }

    func overlaps(_ other: ClosedRange<Int>) -> Bool {
        lock.withLock { range.overlaps(other) }
    }
}

/// Suspends a dispatch queue once, and resumes it only if it is suspended:
/// a queue resumed more often than suspended crashes (tests, task 2.0a).
final class QueueHold: @unchecked Sendable {
    // @unchecked: every access holds the lock.
    private let lock = NSLock()
    private var held = false

    func hold(_ queue: DispatchQueue) {
        lock.withLock {
            guard !held else { return }
            held = true
            queue.suspend()
        }
    }

    func release(_ queue: DispatchQueue) {
        lock.withLock {
            guard held else { return }
            held = false
            queue.resume()
        }
    }
}
