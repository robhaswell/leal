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
    /// The cell, if it is already read; `nil` if it would need a read from
    /// the core now. For laying out text ahead of the scroll (task 2.0a).
    func cachedCell(row: Int, column: Int) -> GridCell?
    /// Reads a region off the main thread, ahead of drawing it (task 2.0a).
    func readAhead(rows: Range<Int>, columns: Range<Int>)
}

extension GridDataSource {
    func rowHasMarker(_ row: Int) -> Bool { false }
    func isHatched(row: Int, column: Int) -> Bool { false }
    func cachedCell(row: Int, column: Int) -> GridCell? { cell(row: row, column: column) }
    func readAhead(rows: Range<Int>, columns: Range<Int>) {}
}

/// One row of a tile: the row's whole field count, and its cells in the
/// tile's columns.
struct TileRow: Equatable, Sendable {
    let fieldCount: Int
    let cells: [GridCell]
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

    /// Reads rows and columns off the main thread; `nil` if they can't be
    /// read. Made for each read (`makeBackgroundFetch`), so it reads from
    /// the document as it is when the read is asked for.
    typealias BackgroundFetch = @Sendable (_ rows: Range<Int>, _ columns: Range<Int>) -> [TileRow]?

    private let capacity: Int
    private let fetch: Fetch
    /// Gives a reader for tiles read ahead of the scroll (task 2.0a), or
    /// `nil` if none can be read now.
    var makeBackgroundFetch: (() -> BackgroundFetch?)?
    private var tiles: [Key: Tile] = [:]
    private var clock: UInt64 = 0
    /// Tiles being read ahead.
    private var reading: Set<Key> = []
    /// Bumped by `removeAll`: a tile read ahead before it is dropped.
    private var generation = 0
    /// The most fields any row read so far has had.
    private(set) var widestRow = 0
    /// How many reads there have been, for tests.
    private(set) var fetchCount = 0
    /// How many tiles were read ahead, for tests.
    private(set) var readAheadCount = 0

    init(capacity: Int = 24, fetch: @escaping Fetch) {
        self.capacity = max(1, capacity)
        self.fetch = fetch
    }

    /// Reads ahead, off the main thread, every tile of a region that isn't
    /// read or being read yet (task 2.0a). A tile read from the core costs
    /// about a millisecond on the main thread, most of it turning the
    /// core's reply into Swift values, and scrolling meets a new tile every
    /// few frames. A tile still missing when it is drawn is read there and
    /// then, as before, so nothing waits for these.
    func readAhead(rows: Range<Int>, columns: Range<Int>, loadedRows: Int) {
        let rows = rows.clamped(to: 0..<max(0, loadedRows))
        guard makeBackgroundFetch != nil, !rows.isEmpty, !columns.isEmpty else { return }
        // Column blocks past every row read so far have nothing to read.
        let columnBlocks = max(1, (widestRow + Self.columnsPerTile - 1) / Self.columnsPerTile)
        let firstColumnBlock = max(0, columns.lowerBound) / Self.columnsPerTile
        let lastColumnBlock = min(columnBlocks - 1, (columns.upperBound - 1) / Self.columnsPerTile)
        guard firstColumnBlock <= lastColumnBlock else { return }
        for rowBlock in (rows.lowerBound / Self.rowsPerTile)...((rows.upperBound - 1) / Self.rowsPerTile) {
            for columnBlock in firstColumnBlock...lastColumnBlock {
                let key = Key(rowBlock: rowBlock, columnBlock: columnBlock)
                guard tiles[key] == nil, !reading.contains(key), let read = makeBackgroundFetch?() else { continue }
                reading.insert(key)
                let start = rowBlock * Self.rowsPerTile
                let columnStart = columnBlock * Self.columnsPerTile
                let tileRows = start..<(start + Self.rowsPerTile)
                let tileColumns = columnStart..<(columnStart + Self.columnsPerTile)
                let generation = generation
                Self.readQueue.async { [weak self] in
                    let result = read(tileRows, tileColumns)
                    Task { @MainActor [weak self] in
                        self?.arrived(key, result, generation: generation)
                    }
                }
            }
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

    private static let readQueue = DispatchQueue(label: "io.github.robhaswell.leal.read-ahead", qos: .userInitiated)

    private func arrived(_ key: Key, _ rows: [TileRow]?, generation: Int) {
        reading.remove(key)
        // A failed read is left for the draw to read again, which reports
        // the error as any core call does.
        guard generation == self.generation, let rows, tiles[key] == nil else { return }
        clock += 1
        readAheadCount += 1
        tiles[key] = Tile(rows: rows, requested: Self.rowsPerTile, failed: false, lastUse: clock)
        for row in rows where row.fieldCount > widestRow {
            widestRow = row.fieldCount
        }
        evictIfNeeded()
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
        for rowBlock in (rows.lowerBound / Self.rowsPerTile)...((rows.upperBound - 1) / Self.rowsPerTile) {
            for columnBlock in (columns.lowerBound / Self.columnsPerTile)...((columns.upperBound - 1) / Self.columnsPerTile) {
                _ = tile(for: Key(rowBlock: rowBlock, columnBlock: columnBlock), loadedRows: loadedRows)
            }
        }
        // The tiles next to it, above and below, and either side.
        let block = Self.rowsPerTile
        readAhead(rows: max(0, rows.lowerBound - block)..<(rows.upperBound + block), columns: columns, loadedRows: loadedRows)
        readAhead(rows: rows, columns: max(0, columns.lowerBound - Self.columnsPerTile)..<(columns.upperBound + Self.columnsPerTile), loadedRows: loadedRows)
    }

    func removeAll() {
        tiles.removeAll()
        reading.removeAll()
        generation += 1
        widestRow = 0
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
