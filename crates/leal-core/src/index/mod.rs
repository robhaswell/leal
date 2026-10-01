//! The row index: where every row of the file starts (DESIGN §3.3).
//!
//! One pass over the bytes records the first byte of each row, as `u32`
//! offsets into the file as stored (ADR-0003 decision 6). It jumps between
//! quote, CR and LF bytes with `memchr`, and tracks whether it is inside a
//! quoted field, so a newline inside quotes doesn't start a row. The same
//! pass counts each row's fields, for the dominant field count (ADR-0003
//! decision 4).
//!
//! # Rules
//!
//! These are DESIGN §3.4 and ADR-0003, as the testkit's generator and the
//! corpus sidecars pin them:
//!
//! - A quote is special only as the **first byte of a field**: at the start
//!   of a row, or straight after a delimiter. Anywhere else it is literal.
//! - Inside a quoted field, `""` is an escaped quote and any other quote
//!   closes the field. Delimiters, CR and LF inside are literal. Text after
//!   the closing quote runs to the next delimiter or line ending, and a
//!   quote in it is literal (ADR-0003 decision 3).
//! - Outside quotes, CRLF, a lone CR and a lone LF each end a row.
//! - A quoted field that never closes runs to the end of the file
//!   ([`RowIndex::unterminated_quote`]).
//! - The BOM belongs to no row: row 0 starts at
//!   [`IndexDialect::bom_len`]. A line ending at the very end of the file
//!   ends the last row and doesn't start another, so an empty or BOM-only
//!   file has no rows (ADR-0003 decision 5).
//! - A **blank line** is a row with no bytes before its line ending. It has
//!   one empty field and is left out of the dominant field count.
//! - In UTF-16 every rule applies to code units, not bytes. The 0x0A byte
//!   inside U+0A22 is not a line ending. Offsets are still byte offsets.
//!
//! # Progressive indexing
//!
//! Indexing runs on a background thread while the app shows the rows found
//! so far (DESIGN §3.10). [`RowIndex::start`] gives a shared [`RowIndex`]
//! for readers and an [`Indexer`] for the thread that does the work:
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use std::sync::Arc;
//! use leal_core::index::{CodeUnit, IndexDialect, RowIndex, Status};
//!
//! let bytes: Arc<[u8]> = Arc::from(&b"id,name\n1,\"Smith,\nJo\"\n"[..]);
//! let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
//! let (index, indexer) = RowIndex::start(dialect)?;
//! let cancel = Arc::new(AtomicBool::new(false));
//!
//! let worker = std::thread::spawn({
//!     let (bytes, cancel) = (Arc::clone(&bytes), Arc::clone(&cancel));
//!     move || indexer.run(&bytes, &cancel, |progress| {
//!         // Tell the UI that `progress.rows` rows are ready.
//!         let _ = progress.rows;
//!     })
//! });
//! // Meanwhile, readers see the rows indexed so far.
//! let _rows_so_far = index.row_count();
//! worker.join().expect("indexer thread")?;
//!
//! assert_eq!(index.status(), Status::Complete);
//! assert_eq!(index.row_count(), 2);
//! assert_eq!(index.row(1, &bytes).map(|r| r.span), Some(8..21));
//! assert_eq!(index.field_count_mode(), Some(2));
//! # Ok::<(), leal_core::index::IndexError>(())
//! ```
//!
//! The indexer works in chunks of [`CHUNK_BYTES`]. After each chunk it
//! publishes the rows it finished, calls the progress callback, and checks
//! the cancel flag (ADR-0005 decision 6, DESIGN §3.10 rule 3). A cancelled
//! run returns [`IndexError::Cancelled`]; the rows published before it stay
//! readable, and the index's status becomes [`Status::Stopped`].
//!
//! [`RowIndex::build`] does the same on the calling thread, for callers that
//! don't need progress (tests, the CLI).
//!
//! # The seam with dialect detection (1.2)
//!
//! The index takes the few facts it needs as plain values in
//! [`IndexDialect`], so that it doesn't depend on 1.2's types. When 1.3a
//! joins them, the values come from 1.2's detection result:
//!
//! - `delimiter`: the detected (or user-chosen) delimiter's byte;
//! - `quote`: always `"` in v1 (DESIGN §3.2);
//! - `code_unit`: [`CodeUnit::Utf16Le`] or [`CodeUnit::Utf16Be`] for the
//!   UTF-16 encodings, [`CodeUnit::Byte`] for UTF-8 and every single-byte
//!   encoding (they are ASCII-compatible, so the structural bytes can't
//!   occur inside a character: ADR-0005 decision 5);
//! - `bom_len`: the length of the BOM that detection found, or 0.
//!
//! [`LineEnding`] is the `dialect` module's (task 1.2), re-exported here.
//! Re-indexing with a different delimiter or encoding is a new
//! [`RowIndex`] over the same bytes: nothing is reopened (PLAN 1.3).
//!
//! # Diagnostics
//!
//! [`RowIndex::build_with_diagnostics`] and
//! [`RowIndex::start_with_diagnostics`] (or [`Indexer::with_diagnostics`],
//! later) also collect the file's [diagnostics](crate::diagnostics) in the
//! same pass (DESIGN §3.3, §3.5).
//! They need the file's text encoding as well as the dialect, for invalid
//! text and NUL code units. [`RowIndex::build`] and [`RowIndex::start`]
//! skip them.
//!
//! # Not in this pass
//!
//! The **whole-file encoding count** (ADR-0003 decision 1) runs later as P2
//! work, not here (ADR-0005 decision 4).

#[cfg(test)]
mod chunked_tests;
pub(crate) mod scan;
#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use scan::{Bytes, NoObserver, RowObserver, Utf16};

use crate::diagnostics::{Collector, Diagnostics, Report};
use crate::dialect::Encoding;

/// How much of the file the indexer scans between publishing rows and
/// checking the cancel flag: 1 MiB, about 1 ms of work on the development
/// Mac and a few on a base M1 Air, inside DESIGN §3.10's ~5 ms chunks.
pub const CHUNK_BYTES: usize = 1 << 20;

/// The chunk size when diagnostics are collected too
/// ([`RowIndex::start_with_diagnostics`]): 256 KiB. In the worst files, where
/// every field or every row is an occurrence, diagnostics cost up to about
/// 6 ns per row or field on the development Mac, so a 1 MiB chunk could take
/// 7 ms there and more on a base M1 Air. A quarter of that stays inside
/// DESIGN §3.10's ~5 ms chunks (rule 3), and costs nothing measurable on
/// ordinary files (`docs/tasks/1.5.md`).
pub const DIAGNOSTICS_CHUNK_BYTES: usize = 256 << 10;

/// The largest file the index can hold: offsets are `u32` (DESIGN §3.3), and
/// files of 4 GiB or more are out of scope (DESIGN §1).
pub const MAX_FILE_BYTES: usize = u32::MAX as usize;

/// What the index needs to know about how the file is written. See the
/// module docs for where 1.2's detection supplies each value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexDialect {
    /// The delimiter, as an ASCII byte (or UTF-16 code unit value).
    pub delimiter: u8,
    /// The quote character, as an ASCII byte (or UTF-16 code unit value).
    pub quote: u8,
    /// How characters are stored.
    pub code_unit: CodeUnit,
    /// The length of the BOM at the start of the file, or 0. Row 0 starts
    /// here.
    pub bom_len: usize,
}

/// How the file stores characters, as far as finding structure goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodeUnit {
    /// One byte per structural character: UTF-8 and the single-byte
    /// encodings.
    Byte,
    /// UTF-16, little-endian: two bytes per code unit, low byte first.
    Utf16Le,
    /// UTF-16, big-endian: two bytes per code unit, high byte first.
    Utf16Be,
}

impl CodeUnit {
    /// The size of one code unit in bytes: 1 or 2.
    #[must_use]
    pub const fn width(self) -> usize {
        match self {
            CodeUnit::Byte => 1,
            CodeUnit::Utf16Le | CodeUnit::Utf16Be => 2,
        }
    }
}

/// The line ending type is detection's ([`crate::dialect::LineEnding`]),
/// re-exported so `index::LineEnding` keeps working.
pub use crate::dialect::LineEnding;

/// Where one row is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowSpan {
    /// The row's bytes, *excluding* its line ending.
    pub span: Range<usize>,
    /// The line ending that ends the row, or `None` for a last row that runs
    /// to the end of the file.
    pub line_ending: Option<LineEnding>,
}

/// Where indexing has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The indexer is still running (or hasn't started). Only rows indexed
    /// so far are visible.
    Indexing,
    /// Every row is indexed.
    Complete,
    /// The indexer stopped before the end: it was cancelled, failed, or was
    /// dropped without running. The rows published before it stopped are
    /// still readable.
    Stopped,
}

/// What the progress callback is told after each chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Rows indexed so far: their spans are final.
    pub rows: usize,
    /// How far into the file the scan has got, in bytes.
    pub bytes_scanned: usize,
    /// The file's length in bytes.
    pub bytes_total: usize,
}

/// Why indexing didn't finish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IndexError {
    /// The delimiter and quote must be different ASCII characters, and
    /// neither may be CR or LF.
    InvalidDialect {
        /// The delimiter given.
        delimiter: u8,
        /// The quote given.
        quote: u8,
    },
    /// The BOM length is longer than the file.
    BomPastEnd {
        /// The BOM length given.
        bom_len: usize,
        /// The file's length.
        len: usize,
    },
    /// The file is larger than [`MAX_FILE_BYTES`].
    TooLarge {
        /// The file's length.
        len: usize,
    },
    /// The encoding given for diagnostics doesn't store characters the way
    /// the dialect says: UTF-16 needs UTF-16 code units, and every other
    /// encoding needs bytes.
    EncodingMismatch {
        /// The dialect's code unit.
        code_unit: CodeUnit,
        /// The encoding given.
        encoding: Encoding,
    },
    /// The cancel flag was set.
    Cancelled,
    /// A [`ChunkedIndexer`] was given more bytes than the file's length,
    /// or finished before it had them all.
    WrongLength {
        /// The file's length, as given to [`Indexer::chunked`].
        expected: usize,
        /// The bytes given so far.
        received: usize,
    },
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::InvalidDialect { delimiter, quote } => write!(
                f,
                "can't index with delimiter {:?} and quote {:?}: they must be different ASCII characters other than CR and LF",
                char::from(*delimiter),
                char::from(*quote)
            ),
            IndexError::BomPastEnd { bom_len, len } => {
                write!(f, "a {bom_len}-byte BOM doesn't fit in a {len}-byte file")
            }
            IndexError::TooLarge { len } => write!(
                f,
                "the file is {len} bytes; Leal reads files of up to {MAX_FILE_BYTES} bytes"
            ),
            IndexError::EncodingMismatch {
                code_unit,
                encoding,
            } => write!(
                f,
                "can't collect diagnostics in {encoding:?} for a file read as {code_unit:?} code units"
            ),
            IndexError::Cancelled => f.write_str("indexing was cancelled"),
            IndexError::WrongLength { expected, received } => write!(
                f,
                "the index was given {received} bytes of a {expected}-byte file"
            ),
        }
    }
}

impl std::error::Error for IndexError {}

/// Where every row starts. Shared between the indexer and its readers; see
/// the module docs.
#[derive(Debug)]
pub struct RowIndex {
    dialect: IndexDialect,
    state: RwLock<State>,
}

/// What readers see. The indexer adds to it once per chunk.
#[derive(Debug)]
struct State {
    /// The start of every row indexed so far, then one more offset: the
    /// start of the row being scanned, or, once complete, the end of the
    /// file. So row `r` with its line ending is `starts[r]..starts[r + 1]`.
    /// Empty before the indexer starts.
    starts: Vec<u32>,
    /// The file's length, once the indexer has started.
    len: usize,
    /// How far the scan has got.
    scanned: usize,
    status: Status,
    /// The most common field count among non-blank rows so far.
    field_count_mode: Option<usize>,
    /// The opening quote of a quoted field that never closes. Set when
    /// indexing completes.
    unterminated_quote: Option<usize>,
}

/// The worker half of [`RowIndex::start`]: it fills the index when it
/// [`run`](Indexer::run)s. Dropping it without finishing leaves the index
/// [`Status::Stopped`].
#[derive(Debug)]
pub struct Indexer {
    index: Arc<RowIndex>,
    chunk_bytes: usize,
    /// Where to publish diagnostics, if they are collected.
    diagnostics: Option<Arc<Diagnostics>>,
}

impl RowIndex {
    /// Indexes `bytes` on this thread and returns the complete index.
    ///
    /// # Errors
    ///
    /// [`IndexError::InvalidDialect`], [`IndexError::BomPastEnd`] or
    /// [`IndexError::TooLarge`].
    ///
    /// ```
    /// use leal_core::index::{CodeUnit, IndexDialect, LineEnding, RowIndex};
    ///
    /// let bytes = b"a;b\r\n\"x\ny\";z\r\n";
    /// let dialect = IndexDialect { delimiter: b';', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
    /// let index = RowIndex::build(bytes, dialect)?;
    /// assert_eq!(index.row_count(), 2);
    /// let row = index.row(1, bytes).unwrap();
    /// assert_eq!((row.span, row.line_ending), (5..12, Some(LineEnding::Crlf)));
    /// # Ok::<(), leal_core::index::IndexError>(())
    /// ```
    pub fn build(bytes: &[u8], dialect: IndexDialect) -> Result<RowIndex, IndexError> {
        let index = RowIndex::new(dialect)?;
        fill(
            &index,
            bytes,
            &AtomicBool::new(false),
            CHUNK_BYTES,
            |_| {},
            &mut NoObserver,
        )?;
        Ok(index)
    }

    /// An empty index for readers, and the [`Indexer`] that fills it. Move
    /// the indexer to a background thread and call [`Indexer::run`].
    ///
    /// # Errors
    ///
    /// [`IndexError::InvalidDialect`].
    pub fn start(dialect: IndexDialect) -> Result<(Arc<RowIndex>, Indexer), IndexError> {
        let index = Arc::new(RowIndex::new(dialect)?);
        let indexer = Indexer {
            index: Arc::clone(&index),
            chunk_bytes: CHUNK_BYTES,
            diagnostics: None,
        };
        Ok((index, indexer))
    }

    /// Indexes `bytes` on this thread, collecting the file's diagnostics in
    /// the same pass, and returns the complete index and the diagnostics.
    /// `encoding` is the file's text encoding.
    ///
    /// # Errors
    ///
    /// [`IndexError::InvalidDialect`], [`IndexError::EncodingMismatch`],
    /// [`IndexError::BomPastEnd`] or [`IndexError::TooLarge`].
    pub fn build_with_diagnostics(
        bytes: &[u8],
        dialect: IndexDialect,
        encoding: Encoding,
    ) -> Result<(RowIndex, Report), IndexError> {
        let index = RowIndex::new(dialect)?;
        let diagnostics = Arc::new(Diagnostics::new(dialect, encoding)?);
        fill(
            &index,
            bytes,
            &AtomicBool::new(false),
            CHUNK_BYTES,
            |_| {},
            &mut Collector::new(Arc::clone(&diagnostics)),
        )?;
        // The collector is gone, so this is the only `Arc` left, and the
        // report moves out without a copy.
        let report = Arc::try_unwrap(diagnostics).map_or_else(
            |shared| Arc::unwrap_or_clone(shared.report()),
            Diagnostics::into_report,
        );
        Ok((index, report))
    }

    /// Like [`RowIndex::start`], but the [`Indexer`] also collects the
    /// file's diagnostics, whose text is in `encoding`. Readers get the
    /// diagnostics found so far from the shared [`Diagnostics`], which the
    /// indexer updates after each chunk, before it calls its progress
    /// callback. See [`crate::diagnostics`]. Its chunks are
    /// [`DIAGNOSTICS_CHUNK_BYTES`], not [`CHUNK_BYTES`], so it publishes,
    /// calls back and checks the cancel flag four times as often.
    ///
    /// It is [`RowIndex::start`] then [`Indexer::with_diagnostics`].
    ///
    /// # Errors
    ///
    /// [`IndexError::InvalidDialect`] or [`IndexError::EncodingMismatch`].
    pub fn start_with_diagnostics(
        dialect: IndexDialect,
        encoding: Encoding,
    ) -> Result<(Arc<RowIndex>, Arc<Diagnostics>, Indexer), IndexError> {
        let (index, indexer) = RowIndex::start(dialect)?;
        let (indexer, diagnostics) = indexer.with_diagnostics(encoding)?;
        Ok((index, diagnostics, indexer))
    }

    fn new(dialect: IndexDialect) -> Result<RowIndex, IndexError> {
        let IndexDialect {
            delimiter, quote, ..
        } = dialect;
        let usable = |b: u8| b.is_ascii() && b != b'\r' && b != b'\n';
        if !usable(delimiter) || !usable(quote) || delimiter == quote {
            return Err(IndexError::InvalidDialect { delimiter, quote });
        }
        Ok(RowIndex {
            dialect,
            state: RwLock::new(State {
                starts: Vec::new(),
                len: 0,
                scanned: 0,
                status: Status::Indexing,
                field_count_mode: None,
                unterminated_quote: None,
            }),
        })
    }

    /// The dialect this index was built with.
    #[must_use]
    pub fn dialect(&self) -> IndexDialect {
        self.dialect
    }

    /// Where indexing has got to.
    #[must_use]
    pub fn status(&self) -> Status {
        self.read().status
    }

    /// The number of rows indexed so far. Once [`Status::Complete`], the
    /// number of rows in the file.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.read().starts.len().saturating_sub(1)
    }

    /// Row `row` *including* its line ending, or `None` if it isn't indexed
    /// (yet). Consecutive extents tile the file after the BOM.
    #[must_use]
    pub fn row_extent(&self, row: usize) -> Option<Range<usize>> {
        let state = self.read();
        let start = *state.starts.get(row)?;
        let next = *state.starts.get(row + 1)?;
        Some(to_usize(start)..to_usize(next))
    }

    /// Row `row`'s span *excluding* its line ending, and the line ending, or
    /// `None` if it isn't indexed (yet).
    ///
    /// `bytes` must be the whole file that was indexed: the line ending is
    /// read from them, so the index stores only one `u32` per row. Bytes of
    /// any other length, such as first paint's first 64 KB, give `None`
    /// rather than spans read from the wrong bytes. (A slice of the right
    /// length but different content can't be detected cheaply; callers pass
    /// the same `Source`'s bytes.)
    #[must_use]
    pub fn row(&self, row: usize, bytes: &[u8]) -> Option<RowSpan> {
        let state = self.read();
        if bytes.len() != state.len {
            return None;
        }
        self.row_from(&state, row, bytes, 0)
    }

    /// [`row`](Self::row), reading the line ending from `window`: the
    /// file's bytes from offset `base` on, rather than the whole file. This
    /// is for a file that has no single slice yet (one on a removable drive,
    /// ADR-0006), whose rows are read with
    /// [`Source::read_range`](crate::source::Source::read_range), one range
    /// per screenful.
    ///
    /// `None` if the row isn't indexed (yet), or `window` doesn't hold all
    /// of its [extent](Self::row_extent). As with `row`, the window must
    /// come from the file that was indexed.
    ///
    /// ```
    /// use leal_core::index::{CodeUnit, IndexDialect, LineEnding, RowIndex};
    ///
    /// let bytes = b"id,name\r\n1,Ada\r\n2,Bob\r\n";
    /// let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
    /// let index = RowIndex::build(bytes, dialect)?;
    /// let extent = index.row_extent(1).unwrap();
    /// // Only row 1's bytes, as a read of its extent gives them.
    /// let window = &bytes[extent.clone()];
    /// let row = index.row_in(1, window, extent.start).unwrap();
    /// assert_eq!((row.span, row.line_ending), (9..14, Some(LineEnding::Crlf)));
    /// assert_eq!(index.row_in(2, window, extent.start), None);
    /// # Ok::<(), leal_core::index::IndexError>(())
    /// ```
    #[must_use]
    pub fn row_in(&self, row: usize, window: &[u8], base: usize) -> Option<RowSpan> {
        self.row_from(&self.read(), row, window, base)
    }

    fn row_from(&self, state: &State, row: usize, window: &[u8], base: usize) -> Option<RowSpan> {
        let start = to_usize(*state.starts.get(row)?);
        let next = to_usize(*state.starts.get(row + 1)?);
        if start < base || next > base.saturating_add(window.len()) {
            return None;
        }
        let is_last = state.status == Status::Complete && row + 2 == state.starts.len();
        if is_last && state.unterminated_quote.is_some() {
            // The last row's final newline, if any, is inside the field.
            return Some(RowSpan {
                span: start..next,
                line_ending: None,
            });
        }
        let line_ending = scan::line_ending_before(window, base, self.dialect, start, next);
        let len = line_ending.map_or(0, |le| scan::line_ending_len(le, self.dialect.code_unit));
        Some(RowSpan {
            span: start..next - len,
            line_ending,
        })
    }

    /// The row whose extent (line ending included) holds byte `offset`, or
    /// `None` if that row isn't indexed (yet), or `offset` is before the
    /// first row (in the BOM). A binary search.
    ///
    /// ```
    /// use leal_core::index::{CodeUnit, IndexDialect, RowIndex};
    ///
    /// let bytes = b"id,name\n1,Ada\n2,Bob\n";
    /// let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
    /// let index = RowIndex::build(bytes, dialect)?;
    /// assert_eq!(index.row_at_offset(0), Some(0));
    /// assert_eq!(index.row_at_offset(7), Some(0)); // its line ending
    /// assert_eq!(index.row_at_offset(8), Some(1));
    /// assert_eq!(index.row_at_offset(20), None);
    /// # Ok::<(), leal_core::index::IndexError>(())
    /// ```
    #[must_use]
    pub fn row_at_offset(&self, offset: usize) -> Option<usize> {
        let state = self.read();
        let after = state
            .starts
            .partition_point(|&start| to_usize(start) <= offset);
        let row = after.checked_sub(1)?;
        (row + 1 < state.starts.len()).then_some(row)
    }

    /// The extent of rows `rows` together, from the first one's start to
    /// the last one's end, including its line ending: the one range to read
    /// for a screenful. `None` if `rows` is empty or its last row isn't
    /// indexed (yet).
    #[must_use]
    pub fn rows_extent(&self, rows: Range<usize>) -> Option<Range<usize>> {
        let state = self.read();
        let last = rows.end.checked_sub(1).filter(|&last| last >= rows.start)?;
        let start = *state.starts.get(rows.start)?;
        let end = *state.starts.get(last + 1)?;
        Some(to_usize(start)..to_usize(end))
    }

    /// The most common field count among non-blank rows, with ties going to
    /// the count seen first (ADR-0003 decision 4), or `None` if there are no
    /// non-blank rows. While indexing, this covers the rows indexed so far,
    /// so it can still change.
    #[must_use]
    pub fn field_count_mode(&self) -> Option<usize> {
        self.read().field_count_mode
    }

    /// The offset of the opening quote of a quoted field that never closes,
    /// if the file has one. Such a field is the last field of the last row.
    /// Known only once indexing is complete.
    #[must_use]
    pub fn unterminated_quote(&self) -> Option<usize> {
        self.read().unterminated_quote
    }

    /// How far the scan has got, in bytes.
    #[must_use]
    pub fn bytes_scanned(&self) -> usize {
        self.read().scanned
    }

    /// An estimate of the file's total row count, for sizing the scrollbar
    /// while indexing (DESIGN §3.10 rule 5): the file's size divided by the
    /// average row length so far. Exact once complete. `None` until a row
    /// has been indexed.
    #[must_use]
    pub fn estimated_row_count(&self) -> Option<usize> {
        let state = self.read();
        let rows = state.starts.len().checked_sub(1).filter(|&r| r > 0)?;
        if state.status == Status::Complete {
            return Some(rows);
        }
        let first = to_usize(*state.starts.first()?);
        let indexed = to_usize(*state.starts.last()?) - first;
        let total = state.len - first;
        let estimate = (rows as u128 * total as u128).div_ceil(indexed.max(1) as u128);
        Some(usize::try_from(estimate).unwrap_or(usize::MAX).max(rows))
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        // A poisoned lock means a thread panicked while holding it. Every
        // write below leaves the state consistent before anything that
        // could panic, so the data is still good to read.
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Resets the state for a run over a `len`-byte file. Row 0 starts at
    /// the end of the BOM.
    fn begin(&self, len: usize) {
        let mut state = self.write();
        state.starts.clear();
        state.starts.push(to_u32(self.dialect.bom_len));
        state.len = len;
        state.scanned = 0;
    }

    /// Publishes one chunk's rows: `new_starts` (then emptied) and how far
    /// the scan got. `done` marks the index complete.
    fn publish(&self, new_starts: &mut Vec<u32>, scan: &scan::Summary, done: bool) -> Progress {
        let mut state = self.write();
        state.starts.extend_from_slice(new_starts);
        new_starts.clear();
        state.scanned = scan.scanned;
        state.field_count_mode = scan.field_count_mode;
        if done {
            state.unterminated_quote = scan.unterminated_quote;
            state.status = Status::Complete;
            state.starts.shrink_to_fit();
        }
        Progress {
            rows: state.starts.len() - 1,
            bytes_scanned: state.scanned,
            bytes_total: state.len,
        }
    }
}

impl Indexer {
    /// This indexer, made to collect the file's diagnostics too, whose text
    /// is in `encoding`, and the shared [`Diagnostics`] it will publish
    /// them to: what [`RowIndex::start_with_diagnostics`] gives, but later.
    /// A document makes its indexer at first paint and calls this on the
    /// index's own thread, so that first paint does no diagnostics work
    /// (DESIGN §3.10 rule 1).
    ///
    /// # Errors
    ///
    /// [`IndexError::EncodingMismatch`] if `encoding` doesn't store
    /// characters the way the index's dialect says. The indexer is then
    /// dropped, so the index is [`Status::Stopped`].
    pub fn with_diagnostics(
        mut self,
        encoding: Encoding,
    ) -> Result<(Indexer, Arc<Diagnostics>), IndexError> {
        let diagnostics = Arc::new(Diagnostics::new(self.index.dialect(), encoding)?);
        self.diagnostics = Some(Arc::clone(&diagnostics));
        self.chunk_bytes = DIAGNOSTICS_CHUNK_BYTES;
        Ok((self, diagnostics))
    }

    /// Indexes `bytes`, publishing rows to the [`RowIndex`] as it goes, and
    /// diagnostics to its [`Diagnostics`] if it was made by
    /// [`RowIndex::start_with_diagnostics`]. After each chunk of
    /// [`CHUNK_BYTES`] (with diagnostics, [`DIAGNOSTICS_CHUNK_BYTES`]) it
    /// calls `on_progress` and checks `cancel`; once `cancel` is set, it
    /// stops at the next chunk boundary.
    ///
    /// # Errors
    ///
    /// [`IndexError::Cancelled`] if `cancel` was set, or
    /// [`IndexError::BomPastEnd`] or [`IndexError::TooLarge`]. The index's
    /// status is then [`Status::Stopped`], and the last diagnostics report
    /// stays incomplete.
    pub fn run(
        self,
        bytes: &[u8],
        cancel: &AtomicBool,
        on_progress: impl FnMut(Progress),
    ) -> Result<(), IndexError> {
        match &self.diagnostics {
            Some(diagnostics) => fill(
                &self.index,
                bytes,
                cancel,
                self.chunk_bytes,
                on_progress,
                &mut Collector::new(Arc::clone(diagnostics)),
            ),
            None => fill(
                &self.index,
                bytes,
                cancel,
                self.chunk_bytes,
                on_progress,
                &mut NoObserver,
            ),
        }
    }

    /// Scans `chunk_bytes` at a time instead of [`CHUNK_BYTES`], so tests
    /// can put chunk boundaries anywhere.
    #[cfg(test)]
    pub(crate) fn with_chunk_bytes(mut self, chunk_bytes: usize) -> Self {
        self.chunk_bytes = chunk_bytes;
        self
    }
}

impl Indexer {
    /// Indexes a `len`-byte file that arrives in chunks, in order, rather
    /// than as one slice: a file on a removable drive, indexed from
    /// [`Source::stream`](crate::source::Source::stream) in the same pass
    /// that copies it to the internal disk (ADR-0006). Give it each chunk
    /// with [`ChunkedIndexer::push`], then call
    /// [`ChunkedIndexer::finish`]. The rows are exactly those
    /// [`run`](Self::run) finds over the whole file, wherever the chunks
    /// are cut.
    ///
    /// ```
    /// use leal_core::index::{CodeUnit, IndexDialect, RowIndex, Status};
    ///
    /// let bytes = b"a,\"x\r\ny\"\r\nb,c\r\n";
    /// let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
    /// let (index, indexer) = RowIndex::start(dialect)?;
    /// let mut chunked = indexer.chunked(bytes.len())?;
    /// // Cut inside the quoted CRLF, and between the last CR and LF.
    /// for chunk in [&bytes[..5], &bytes[5..14], &bytes[14..]] {
    ///     chunked.push(chunk)?;
    /// }
    /// chunked.finish()?;
    /// assert_eq!(index.status(), Status::Complete);
    /// assert_eq!(index.row_count(), 2);
    /// assert_eq!(index.row(0, bytes).map(|r| r.span), Some(0..8));
    /// # Ok::<(), leal_core::index::IndexError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`IndexError::TooLarge`] or [`IndexError::BomPastEnd`].
    pub fn chunked(self, len: usize) -> Result<ChunkedIndexer, IndexError> {
        check_len(len)?;
        let dialect = self.index.dialect;
        if dialect.bom_len > len {
            return Err(IndexError::BomPastEnd {
                bom_len: dialect.bom_len,
                len,
            });
        }
        let scan = match dialect.code_unit {
            CodeUnit::Byte => ChunkedScan::Bytes(scan::Chunked::new(&self.index, len)),
            CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
                ChunkedScan::Utf16(scan::Chunked::new(&self.index, len))
            }
        };
        let collector = self
            .diagnostics
            .as_ref()
            .map(|diagnostics| Collector::new(Arc::clone(diagnostics)));
        Ok(ChunkedIndexer {
            indexer: self,
            scan,
            len,
            collector,
        })
    }
}

/// An [`Indexer`] that is given the file in chunks: see
/// [`Indexer::chunked`]. Dropping it before [`finish`](Self::finish) leaves
/// the index [`Status::Stopped`], with the rows found so far.
///
/// If the indexer was made by [`RowIndex::start_with_diagnostics`], it
/// collects diagnostics chunk by chunk, exactly as [`Indexer::run`] does over
/// the whole file: the collector's state, like the scanner's, carries over
/// from one chunk to the next, and a UTF-8 sequence or UTF-16 pair cut by a
/// chunk boundary is judged once the next chunk arrives.
pub struct ChunkedIndexer {
    indexer: Indexer,
    scan: ChunkedScan,
    len: usize,
    /// The diagnostics collector, if the indexer has diagnostics.
    collector: Option<Collector>,
}

/// The chunked scan, for the file's code unit.
enum ChunkedScan {
    Bytes(scan::Chunked<Bytes>),
    Utf16(scan::Chunked<Utf16>),
}

impl ChunkedIndexer {
    /// Scans the next chunk of the file and publishes the rows it finished,
    /// as [`Indexer::run`] does after each of its chunks. The chunk may be
    /// any length, even empty; a chunk can end in the middle of a CRLF, a
    /// `""` or a UTF-16 code unit.
    ///
    /// # Errors
    ///
    /// [`IndexError::WrongLength`] if the chunks so far would be longer
    /// than the file. The chunk is then not scanned.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Progress, IndexError> {
        // The diagnostics collector joins the pass here, as it joins
        // `fill` for a mapped file.
        match self.collector.take() {
            Some(mut collector) => {
                let progress = self.push_observed(chunk, &mut collector);
                self.collector = Some(collector);
                progress
            }
            None => self.push_observed(chunk, &mut NoObserver),
        }
    }

    /// [`push`](Self::push), telling `observer` about every row it finds.
    pub(crate) fn push_observed<O: RowObserver>(
        &mut self,
        chunk: &[u8],
        observer: &mut O,
    ) -> Result<Progress, IndexError> {
        let received = self.received().saturating_add(chunk.len());
        if received > self.len {
            return Err(IndexError::WrongLength {
                expected: self.len,
                received,
            });
        }
        let index = &self.indexer.index;
        Ok(match &mut self.scan {
            ChunkedScan::Bytes(scan) => scan.push(index, chunk, observer),
            ChunkedScan::Utf16(scan) => scan.push(index, chunk, observer),
        })
    }

    /// How many bytes have been given so far.
    #[must_use]
    pub fn received(&self) -> usize {
        match &self.scan {
            ChunkedScan::Bytes(scan) => scan.received(),
            ChunkedScan::Utf16(scan) => scan.received(),
        }
    }

    /// Ends the last row and marks the index [`Status::Complete`], once
    /// every byte of the file has been given.
    ///
    /// # Errors
    ///
    /// [`IndexError::WrongLength`] if fewer bytes than the file's length
    /// were given. The index is then [`Status::Stopped`].
    pub fn finish(mut self) -> Result<Progress, IndexError> {
        match self.collector.take() {
            Some(mut collector) => self.finish_observed(&mut collector),
            None => self.finish_observed(&mut NoObserver),
        }
    }

    /// [`finish`](Self::finish), telling `observer` about the last row.
    pub(crate) fn finish_observed<O: RowObserver>(
        self,
        observer: &mut O,
    ) -> Result<Progress, IndexError> {
        let received = self.received();
        if received != self.len {
            return Err(IndexError::WrongLength {
                expected: self.len,
                received,
            });
        }
        let ChunkedIndexer { indexer, scan, .. } = self;
        Ok(match scan {
            ChunkedScan::Bytes(scan) => scan.finish(&indexer.index, observer),
            ChunkedScan::Utf16(scan) => scan.finish(&indexer.index, observer),
        })
    }
}

impl fmt::Debug for ChunkedIndexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkedIndexer")
            .field("len", &self.len)
            .field("received", &self.received())
            .finish_non_exhaustive()
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        let mut state = self.index.write();
        if state.status == Status::Indexing {
            state.status = Status::Stopped;
        }
    }
}

/// Checks the inputs, then scans `bytes` chunk by chunk into `index`.
fn fill<O: RowObserver>(
    index: &RowIndex,
    bytes: &[u8],
    cancel: &AtomicBool,
    chunk_bytes: usize,
    on_progress: impl FnMut(Progress),
    observer: &mut O,
) -> Result<(), IndexError> {
    let len = bytes.len();
    check_len(len)?;
    let dialect = index.dialect;
    if dialect.bom_len > len {
        return Err(IndexError::BomPastEnd {
            bom_len: dialect.bom_len,
            len,
        });
    }
    match dialect.code_unit {
        CodeUnit::Byte => {
            scan::run::<Bytes, O>(index, bytes, cancel, chunk_bytes, on_progress, observer)
        }
        CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
            scan::run::<Utf16, O>(index, bytes, cancel, chunk_bytes, on_progress, observer)
        }
    }
}

fn check_len(len: usize) -> Result<(), IndexError> {
    if len > MAX_FILE_BYTES {
        Err(IndexError::TooLarge { len })
    } else {
        Ok(())
    }
}

/// An offset as stored. Every offset is at most the file's length, which
/// [`check_len`] has checked fits in a `u32`.
fn to_u32(offset: usize) -> u32 {
    debug_assert!(offset <= MAX_FILE_BYTES);
    u32::try_from(offset).unwrap_or(u32::MAX)
}

/// A stored offset as a `usize`. Lossless: Leal runs only on 64-bit Macs.
fn to_usize(offset: u32) -> usize {
    offset as usize
}
