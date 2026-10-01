//! A small least-recently-used cache of parsed rows, for the rows on screen
//! (DESIGN §3.4).
//!
//! The grid asks for the same rows again and again as it redraws and
//! scrolls a little. The cache keeps the rows read most recently, and when
//! it is full, drops the one read longest ago. "Full" has two limits: a
//! number of rows, and a total number of fields across those rows, so a
//! file with very wide rows can't make the cache large (a `FieldSpan` is
//! 32 bytes, so 256 rows of 10,000 fields would be 82 MB).
//!
//! It is a hash map from row number to parsed row, with a counter that
//! goes up on every read: each entry remembers the counter's value when it
//! was last read, and the oldest one is evicted. Finding it is a scan of
//! the whole map, which for a few hundred rows takes well under a
//! microsecond, and happens only on a miss, next to parsing a row. That is
//! simpler than a linked list and costs nothing that matters.

use std::collections::HashMap;
use std::sync::Arc;

use super::{ParsedRow, RowParser};
use crate::index::RowIndex;

/// The default row limit: several screens of rows, even on a tall display
/// with a small font.
pub const DEFAULT_CACHE_ROWS: usize = 256;

/// The default limit on fields across all cached rows: 100,000 fields of 32
/// bytes each, so at most about 3.2 MB of field spans (plus about 100 bytes
/// of bookkeeping per row). 256 rows of up to 390 fields fit; wider rows
/// mean fewer rows are kept.
pub const DEFAULT_CACHE_FIELDS: usize = 100_000;

/// Parsed rows, by row number, for one [`RowParser`].
///
/// A cache belongs to one index: rows are cached by number, so after
/// re-indexing (with another delimiter or encoding), make a new cache with
/// the new parser. Rows the index has published never change, so while
/// the index is still being built, cached rows stay correct.
#[derive(Debug)]
pub struct RowCache {
    parser: RowParser,
    max_rows: usize,
    max_fields: usize,
    rows: HashMap<usize, Entry>,
    /// The number of fields in all the cached rows.
    fields: usize,
    /// Goes up by one on every read.
    clock: u64,
}

#[derive(Debug)]
struct Entry {
    row: Arc<ParsedRow>,
    /// The clock when the row was last read.
    last_read: u64,
}

impl RowCache {
    /// An empty cache that holds up to `max_rows` rows (at least one) and
    /// [`DEFAULT_CACHE_FIELDS`] fields, parsed with `parser`.
    #[must_use]
    pub fn new(parser: RowParser, max_rows: usize) -> Self {
        RowCache::with_limits(parser, max_rows, DEFAULT_CACHE_FIELDS)
    }

    /// An empty cache that holds up to `max_rows` rows (at least one) with
    /// up to `max_fields` fields between them. A single row with more
    /// fields than `max_fields` is parsed but never kept.
    #[must_use]
    pub fn with_limits(parser: RowParser, max_rows: usize, max_fields: usize) -> Self {
        let max_rows = max_rows.max(1);
        RowCache {
            parser,
            max_rows,
            max_fields,
            rows: HashMap::with_capacity(max_rows),
            fields: 0,
            clock: 0,
        }
    }

    /// The parser rows are parsed with, which also gives their display
    /// values.
    #[must_use]
    pub fn parser(&self) -> RowParser {
        self.parser
    }

    /// The most rows the cache holds.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.max_rows
    }

    /// The most fields the cached rows have between them.
    #[must_use]
    pub fn max_fields(&self) -> usize {
        self.max_fields
    }

    /// How many rows the cache holds now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// How many fields the cached rows have between them now.
    #[must_use]
    pub fn fields(&self) -> usize {
        self.fields
    }

    /// True if the cache holds no rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// True if row `row` is in the cache. Doesn't count as a read.
    #[must_use]
    pub fn contains(&self, row: usize) -> bool {
        self.rows.contains_key(&row)
    }

    /// Drops every row.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.fields = 0;
    }

    /// Row `row`, parsed: from the cache if it is there, otherwise parsed
    /// now and kept (unless it alone has more fields than the cache may
    /// hold). `None` if the index doesn't have the row (yet), or was built
    /// with another dialect; nothing is cached then.
    ///
    /// `bytes` must be the whole file that was indexed. The row comes back
    /// as an [`Arc`], so the caller can keep it while the cache moves on.
    pub fn row(&mut self, index: &RowIndex, row: usize, bytes: &[u8]) -> Option<Arc<ParsedRow>> {
        let parser = self.parser;
        self.row_with(row, || parser.parse_row(index, row, bytes))
    }

    /// [`row`](Self::row), parsing a missing row from `window`, the file's
    /// bytes from offset `base` on, which must hold the row's extent
    /// ([`RowParser::parse_row_in`]). Rows are cached by number, so the
    /// same row comes back however it was first read.
    pub fn row_in(
        &mut self,
        index: &RowIndex,
        row: usize,
        window: &[u8],
        base: usize,
    ) -> Option<Arc<ParsedRow>> {
        let parser = self.parser;
        self.row_with(row, || parser.parse_row_in(index, row, window, base))
    }

    /// Row `row` from the cache, or parsed by `parse` and kept.
    fn row_with(
        &mut self,
        row: usize,
        parse: impl FnOnce() -> Option<ParsedRow>,
    ) -> Option<Arc<ParsedRow>> {
        self.clock += 1;
        if let Some(entry) = self.rows.get_mut(&row) {
            entry.last_read = self.clock;
            return Some(Arc::clone(&entry.row));
        }
        let parsed = Arc::new(parse()?);
        let fields = parsed.fields().len();
        if fields > self.max_fields {
            return Some(parsed);
        }
        while self.rows.len() >= self.max_rows || self.fields + fields > self.max_fields {
            if !self.evict_least_recently_read() {
                break;
            }
        }
        self.fields += fields;
        self.rows.insert(
            row,
            Entry {
                row: Arc::clone(&parsed),
                last_read: self.clock,
            },
        );
        Some(parsed)
    }

    /// Drops the row read longest ago. False if the cache was empty.
    fn evict_least_recently_read(&mut self) -> bool {
        let oldest = self
            .rows
            .iter()
            .min_by_key(|(_, entry)| entry.last_read)
            .map(|(&row, _)| row);
        match oldest.and_then(|row| self.rows.remove(&row)) {
            Some(entry) => {
                self.fields -= entry.row.fields().len();
                true
            }
            None => false,
        }
    }
}
