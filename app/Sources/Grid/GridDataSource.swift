import Foundation

/// What the grid shows in one cell.
enum GridCell: Equatable, Sendable {
    /// A value, perhaps empty. `truncated` says the core gave only its
    /// start (`GridMetrics.maxCellCharacters`).
    case text(String, truncated: Bool)
    /// The row has fewer fields than this column has (a short row). Task
    /// 1.7 draws these hatched once the core says the row is ragged.
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

    private let capacity: Int
    private let fetch: Fetch
    private var tiles: [Key: Tile] = [:]
    private var clock: UInt64 = 0
    /// The most fields any row read so far has had.
    private(set) var widestRow = 0
    /// How many reads there have been, for tests.
    private(set) var fetchCount = 0

    init(capacity: Int = 24, fetch: @escaping Fetch) {
        self.capacity = max(1, capacity)
        self.fetch = fetch
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
    }

    func removeAll() {
        tiles.removeAll()
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
