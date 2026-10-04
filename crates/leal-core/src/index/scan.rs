//! The scanner: one pass over the bytes that finds where rows end and
//! counts each row's fields. See the module docs in `mod.rs` for the rules.
//!
//! The scanner is a small state machine with two states, *outside* and
//! *inside* a quoted field. Instead of looking at every byte, it asks
//! `memchr` for the next byte that matters in the current state:
//!
//! - outside quotes: the next quote, CR or LF (`memchr3`). The delimiters
//!   in between are counted in one go (`count`), for the row's field count;
//! - inside quotes: the next quote (`memchr`). Nothing else matters there.
//!
//! A quote found outside opens a field only if it is the field's first
//! byte: at the row's start, or right after a delimiter. That one-unit look
//! back is enough. Once a quoted field closes, the next unit is never a
//! quote (it would have been `""`), so a quote outside with a delimiter
//! before it really is at a field's start.
//!
//! The code is generic over [`Units`], how the file stores characters, so
//! the same state machine reads bytes (UTF-8 and single-byte encodings)
//! and UTF-16 code units. Rust compiles a separate copy for each, so the
//! byte version pays nothing for UTF-16's extra checks.
//!
//! The scanner keeps every position as an offset into the whole file, but
//! reads the bytes through a [`View`]: a slice that starts at some offset
//! `base`. Over a mapped file the view is the whole file (`base` 0). For a
//! file read in chunks ([`Chunked`], task 1.3a) each view is one chunk plus
//! the few bytes the scanner still needs from the chunk before: the unit
//! before its position (for the "quote after a delimiter" look-back) and
//! anything it hasn't scanned yet. The scanner's own state (inside quotes
//! or not, the row so far) carries over from one view to the next.
//!
//! It is generic over a [`RowObserver`] too, which hears what the scan
//! finds: that is how diagnostics are collected in the same pass
//! (`crate::diagnostics`). Without one ([`NoObserver`]) the hooks compile
//! to nothing.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{CodeUnit, IndexDialect, IndexError, LineEnding, Progress, RowIndex, to_u32};

const CR: u8 = b'\r';
const LF: u8 = b'\n';

// ---------------------------------------------------------------------------
// Code units

/// How to find structural characters in the file's bytes. Every position is
/// a byte offset of the start of a code unit, and every `value` is the
/// ASCII character the code unit holds (delimiter, quote, CR or LF).
pub(crate) trait Units {
    /// The width of a code unit in bytes.
    const WIDTH: usize;

    /// The units of a slice that starts at byte `base` of a file written
    /// in `dialect`. Positions passed to the other methods are then offsets
    /// into that slice.
    fn at(dialect: IndexDialect, base: usize) -> Self;

    /// The first unit in `from..to` whose value is `a`, `b` or `c`, with
    /// that value.
    fn find3(&self, bytes: &[u8], from: usize, to: usize, abc: [u8; 3]) -> Option<(usize, u8)>;

    /// The first unit in `from..to` whose value is `a`.
    fn find1(&self, bytes: &[u8], from: usize, to: usize, a: u8) -> Option<usize>;

    /// True if a whole unit starts at `pos` and holds `value`.
    fn is(&self, bytes: &[u8], pos: usize, value: u8) -> bool;

    /// How many units in `from..to` hold `value`.
    fn count(&self, bytes: &[u8], from: usize, to: usize, value: u8) -> usize;
}

/// One byte per character.
pub(crate) struct Bytes;

impl Units for Bytes {
    const WIDTH: usize = 1;

    fn at(_dialect: IndexDialect, _base: usize) -> Self {
        Bytes
    }

    fn find3(
        &self,
        bytes: &[u8],
        from: usize,
        to: usize,
        [a, b, c]: [u8; 3],
    ) -> Option<(usize, u8)> {
        let i = from + memchr::memchr3(a, b, c, &bytes[from..to])?;
        Some((i, bytes[i]))
    }

    fn find1(&self, bytes: &[u8], from: usize, to: usize, a: u8) -> Option<usize> {
        Some(from + memchr::memchr(a, &bytes[from..to])?)
    }

    fn is(&self, bytes: &[u8], pos: usize, value: u8) -> bool {
        bytes.get(pos) == Some(&value)
    }

    fn count(&self, bytes: &[u8], from: usize, to: usize, value: u8) -> usize {
        // A plain loop: the compiler turns it into SIMD comparisons.
        bytes[from..to].iter().filter(|&&b| b == value).count()
    }
}

/// UTF-16: two bytes per code unit, starting at the end of the BOM.
///
/// `memchr` still does the searching, for the byte that holds an ASCII
/// character's value (the first byte of a unit in little-endian, the
/// second in big-endian). A hit counts only if it is in that position of a
/// unit and the unit's other byte is 0. So the 0x0A byte in U+0A22, or in
/// U+220A, is skipped.
pub(crate) struct Utf16 {
    /// Which bytes of the slice start a code unit: those at an offset `i`
    /// with `i + phase` even. Units start at the end of the BOM, so for
    /// the whole file this is the BOM's length, mod 2.
    phase: usize,
    big_endian: bool,
}

impl Utf16 {
    /// The unit's two bytes for an ASCII value, in file order.
    fn pair(&self, value: u8) -> [u8; 2] {
        if self.big_endian {
            [0, value]
        } else {
            [value, 0]
        }
    }

    /// Searches `from..to` with `search` (a `memchr` over value bytes) and
    /// returns the first hit that is a real code unit, as the unit's start.
    fn find(
        &self,
        bytes: &[u8],
        from: usize,
        to: usize,
        search: impl Fn(&[u8]) -> Option<usize>,
    ) -> Option<usize> {
        let mut at = from;
        while at < to {
            let hit = at + search(&bytes[at..to])?;
            // Where a unit's value byte sits: offset 0 of the unit for
            // little-endian, 1 for big-endian.
            let in_unit = (hit + self.phase) % 2;
            let start = hit - in_unit;
            if in_unit == usize::from(self.big_endian) {
                let other = if self.big_endian { start } else { start + 1 };
                if bytes.get(other) == Some(&0) {
                    return Some(start);
                }
            }
            at = hit + 1;
        }
        None
    }
}

impl Units for Utf16 {
    const WIDTH: usize = 2;

    fn at(dialect: IndexDialect, base: usize) -> Self {
        // A byte at offset `i` of the slice is at `base + i` in the file,
        // which starts a unit if `base + i - bom_len` is even, that is, if
        // `i + base + bom_len` is.
        Utf16 {
            phase: (base + dialect.bom_len) % 2,
            big_endian: dialect.code_unit == CodeUnit::Utf16Be,
        }
    }

    fn find3(
        &self,
        bytes: &[u8],
        from: usize,
        to: usize,
        [a, b, c]: [u8; 3],
    ) -> Option<(usize, u8)> {
        let start = self.find(bytes, from, to, |hay| memchr::memchr3(a, b, c, hay))?;
        let value = bytes[start + usize::from(self.big_endian)];
        Some((start, value))
    }

    fn find1(&self, bytes: &[u8], from: usize, to: usize, a: u8) -> Option<usize> {
        self.find(bytes, from, to, |hay| memchr::memchr(a, hay))
    }

    fn is(&self, bytes: &[u8], pos: usize, value: u8) -> bool {
        bytes.get(pos..pos + 2) == Some(&self.pair(value)[..])
    }

    fn count(&self, bytes: &[u8], from: usize, to: usize, value: u8) -> usize {
        let pair = self.pair(value);
        // `as_chunks` splits the slice into whole units (and ignores a final
        // odd byte, which isn't one).
        let (units, _odd) = bytes[from..to].as_chunks::<2>();
        units.iter().filter(|&&unit| unit == pair).count()
    }
}

/// Part of the file: `bytes` are the file's bytes from offset `base` on.
/// Its methods take and return offsets into the whole file, so the scanner
/// never has to know where the slice starts.
pub(crate) struct View<'a, U> {
    bytes: &'a [u8],
    base: usize,
    units: U,
}

impl<'a, U: Units> View<'a, U> {
    /// The view of `bytes`, which start at offset `base` of a file written
    /// in `dialect`.
    pub(crate) fn new(bytes: &'a [u8], base: usize, dialect: IndexDialect) -> Self {
        View {
            bytes,
            base,
            units: U::at(dialect, base),
        }
    }

    pub(crate) fn find3(&self, from: usize, to: usize, abc: [u8; 3]) -> Option<(usize, u8)> {
        let (at, value) = self
            .units
            .find3(self.bytes, from - self.base, to - self.base, abc)?;
        Some((at + self.base, value))
    }

    pub(crate) fn find1(&self, from: usize, to: usize, a: u8) -> Option<usize> {
        let at = self
            .units
            .find1(self.bytes, from - self.base, to - self.base, a)?;
        Some(at + self.base)
    }

    /// False for a position outside the view, as for one past the end of
    /// the file.
    pub(crate) fn is(&self, pos: usize, value: u8) -> bool {
        pos.checked_sub(self.base)
            .is_some_and(|at| self.units.is(self.bytes, at, value))
    }

    pub(crate) fn count(&self, from: usize, to: usize, value: u8) -> usize {
        self.units
            .count(self.bytes, from - self.base, to - self.base, value)
    }

    /// The file's bytes `from..to`, which must be in the view.
    pub(crate) fn slice(&self, from: usize, to: usize) -> &'a [u8] {
        &self.bytes[from - self.base..to - self.base]
    }

    /// The offset in the file just past the view's last byte.
    pub(crate) fn end(&self) -> usize {
        self.base + self.bytes.len()
    }
}

// ---------------------------------------------------------------------------
// The hook for diagnostics (1.5)

/// What the scanner knows about a row when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowFacts {
    /// The row's number, from 0.
    pub row: usize,
    /// The row's bytes, excluding its line ending. Empty for a blank line.
    pub span: Range<usize>,
    /// The line ending, or `None` for a last row with none.
    pub line_ending: Option<LineEnding>,
    /// Where the next row starts (the end of this one's line ending).
    pub next_start: usize,
    /// The number of fields: one more than the delimiters outside quotes.
    pub fields: usize,
}

/// A stretch of units the scanner has passed over, `from..to`, in row
/// `row`, starting in field `field` (from 0).
///
/// Inside a quoted field (`quoted`), the whole stretch is in that field.
/// Outside quotes, each delimiter in it starts the next field. A row's
/// stretches, with the quotes, CRs and LFs between them, cover the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub row: usize,
    pub field: usize,
    pub from: usize,
    pub to: usize,
    pub quoted: bool,
}

/// Told what the scanner finds, as it finds it. Diagnostics are collected
/// this way, in the same pass (DESIGN §3.3, [`crate::diagnostics`]). Tests
/// use an observer to read each row's field count.
///
/// Every method but [`row`](RowObserver::row) does nothing by default. The
/// scanner is generic over its observer, so with [`NoObserver`] the calls
/// compile to nothing.
pub(crate) trait RowObserver {
    /// How many bytes past the point a scan stops at the observer needs to
    /// see in [`chunk`](RowObserver::chunk)'s view. The chunked scan keeps
    /// that many bytes (at least a unit) back for the next chunk.
    const LOOKAHEAD: usize = 0;

    /// The scanner is about to scan up to `to` of a `len`-byte file, through
    /// `view`, which holds at least [`LOOKAHEAD`](RowObserver::LOOKAHEAD)
    /// bytes past `to` (or up to the end of the file).
    fn chunk<U: Units>(&mut self, _view: &View<'_, U>, _to: usize, _len: usize) {}

    /// The scanner passed over a stretch of units; see [`Segment`].
    fn content<U: Units>(&mut self, _view: &View<'_, U>, _segment: Segment) {}

    /// A quoted field in row `row` closed and has text after its closing
    /// quote, starting at `offset`.
    fn text_after_quote(&mut self, _row: usize, _offset: usize) {}

    /// The quoted field that opens at `offset`, in row `row`, never closes.
    /// Called at the end of the file, just before that row ends.
    fn unterminated_quote(&mut self, _row: usize, _offset: usize) {}

    /// A row ended. `counts` already includes it, unless it is blank.
    fn row(&mut self, facts: &RowFacts, counts: &FieldCounts);

    /// After a chunk, the index published its rows, `rows` of them so far;
    /// `done` if that was the last chunk.
    fn published(&mut self, _rows: usize, _done: bool) {}
}

/// Ignores every row. The compiler removes the calls entirely.
pub(crate) struct NoObserver;

impl RowObserver for NoObserver {
    fn row(&mut self, _facts: &RowFacts, _counts: &FieldCounts) {}
}

// ---------------------------------------------------------------------------
// Field counts

/// Counts how many rows have each field count, to find the most common one
/// (ties go to the count seen first). Most files have one or two distinct
/// counts, and most rows have the same count as the row before, so that
/// case skips the hash map.
#[derive(Debug, Default)]
pub(crate) struct FieldCounts {
    /// `(field count, rows)`, in the order each count was first seen.
    seen: Vec<(usize, u64)>,
    /// Where each field count is in `seen`.
    position: HashMap<usize, usize>,
    /// The position of the last count added.
    last: Option<usize>,
    /// The position of the most common count so far.
    best: Option<usize>,
    /// Rows counted.
    total: u64,
}

impl FieldCounts {
    pub(crate) fn add(&mut self, fields: usize) {
        let i = match self.last {
            Some(i) if self.seen[i].0 == fields => i,
            _ => *self.position.entry(fields).or_insert_with(|| {
                self.seen.push((fields, 0));
                self.seen.len() - 1
            }),
        };
        self.seen[i].1 += 1;
        self.total += 1;
        self.last = Some(i);
        // Only count `i` changed, so it is the only one that can overtake
        // the best. An equal count wins only if it was seen first.
        let n = self.seen[i].1;
        self.best = match self.best {
            Some(b) if self.seen[b].1 > n || (self.seen[b].1 == n && b <= i) => Some(b),
            _ => Some(i),
        };
    }

    pub(crate) fn mode(&self) -> Option<usize> {
        self.best.map(|b| self.seen[b].0)
    }

    /// The most common field count, and how many rows have it.
    pub(crate) fn leader(&self) -> Option<(usize, u64)> {
        self.best.map(|b| self.seen[b])
    }

    /// How many rows have been counted.
    pub(crate) fn total(&self) -> u64 {
        self.total
    }
}

// ---------------------------------------------------------------------------
// The scanner

/// The scan's results that readers see, published after each chunk.
pub(crate) struct Summary {
    pub scanned: usize,
    pub field_count_mode: Option<usize>,
    pub unterminated_quote: Option<usize>,
}

struct Scanner<U> {
    /// The file's length.
    len: usize,
    delimiter: u8,
    quote: u8,
    /// The next unit to look at.
    pos: usize,
    /// Where the current row starts.
    row_start: usize,
    /// The current row's number.
    row: usize,
    /// The current row's fields so far.
    fields: usize,
    /// While inside a quoted field: where its opening quote is.
    open_quote: Option<usize>,
    counts: FieldCounts,
    /// Row starts found since the last chunk was published.
    new_starts: Vec<u32>,
    unterminated_quote: Option<usize>,
    units: PhantomData<U>,
}

impl<U: Units> Scanner<U> {
    fn new(len: usize, dialect: IndexDialect) -> Self {
        Scanner {
            len,
            delimiter: dialect.delimiter,
            quote: dialect.quote,
            pos: dialect.bom_len,
            row_start: dialect.bom_len,
            row: 0,
            fields: 1,
            open_quote: None,
            counts: FieldCounts::default(),
            new_starts: Vec::new(),
            unterminated_quote: None,
            units: PhantomData,
        }
    }

    /// Scans until `pos` reaches `to`, which is on a code unit boundary or
    /// at the end of the file. It can stop a unit past `to`, when it looks
    /// ahead for the LF of a CRLF or the second quote of `""`; the next
    /// chunk carries on from there.
    ///
    /// `view` must hold the unit before `pos` (if there is one), and
    /// everything from `pos` to one unit past `to`, or to the end of the
    /// file if that is sooner.
    fn scan<O: RowObserver>(&mut self, view: &View<'_, U>, to: usize, observer: &mut O) {
        let w = U::WIDTH;
        observer.chunk(view, to, self.len);
        while self.pos < to {
            if self.open_quote.is_some() {
                // Inside a quoted field: only a quote matters.
                let found = view.find1(self.pos, to, self.quote);
                observer.content(view, self.segment(found.unwrap_or(to), true));
                let Some(q) = found else {
                    self.pos = to;
                    break;
                };
                if view.is(q + w, self.quote) {
                    // `""`: an escaped quote.
                    self.pos = q + 2 * w;
                } else {
                    // The closing quote. If the unit after it isn't a
                    // delimiter, CR, LF or the end of the file, the field
                    // has text after its closing quote. In UTF-16 a final
                    // odd byte counts as text, as it does for the row
                    // parser (1.4).
                    self.open_quote = None;
                    self.pos = q + w;
                    let after = q + w;
                    let text_follows = after < self.len
                        && !view.is(after, self.delimiter)
                        && !view.is(after, CR)
                        && !view.is(after, LF);
                    if text_follows {
                        observer.text_after_quote(self.row, after);
                    }
                }
                continue;
            }

            // Outside quotes: the next quote, CR or LF.
            let found = view.find3(self.pos, to, [self.quote, CR, LF]);
            let end = found.map_or(to, |(at, _)| at);
            observer.content(view, self.segment(end, false));
            self.fields += view.count(self.pos, end, self.delimiter);
            let Some((at, value)) = found else {
                self.pos = to;
                break;
            };
            if value == LF {
                self.end_row(at, Some(LineEnding::Lf), at + w, observer);
            } else if value == CR {
                if view.is(at + w, LF) {
                    self.end_row(at, Some(LineEnding::Crlf), at + 2 * w, observer);
                } else {
                    self.end_row(at, Some(LineEnding::Cr), at + w, observer);
                }
            } else {
                // A quote opens a field only as its first unit.
                if at == self.row_start || view.is(at - w, self.delimiter) {
                    self.open_quote = Some(at);
                }
                // Otherwise it is literal: in an unquoted field (`a"b`), or
                // in text after a closing quote (ADR-0003 decision 3).
                self.pos = at + w;
            }
        }
    }

    /// The stretch from `pos` to `to`, in the current row and field.
    fn segment(&self, to: usize, quoted: bool) -> Segment {
        Segment {
            row: self.row,
            field: self.fields - 1,
            from: self.pos,
            to,
            quoted,
        }
    }

    /// The end of the file: the last row, if it has no line ending.
    fn finish<O: RowObserver>(&mut self, observer: &mut O) {
        let len = self.len;
        if let Some(open) = self.open_quote.take() {
            observer.unterminated_quote(self.row, open);
            self.unterminated_quote = Some(open);
            self.end_row(len, None, len, observer);
        } else if self.row_start < len {
            self.end_row(len, None, len, observer);
        }
    }

    /// Ends the current row: its bytes end at `content_end`, then comes
    /// `line_ending`, and the next row starts at `next`.
    fn end_row<O: RowObserver>(
        &mut self,
        content_end: usize,
        line_ending: Option<LineEnding>,
        next: usize,
        observer: &mut O,
    ) {
        let blank = content_end == self.row_start;
        if !blank {
            self.counts.add(self.fields);
        }
        observer.row(
            &RowFacts {
                row: self.row,
                span: self.row_start..content_end,
                line_ending,
                next_start: next,
                fields: self.fields,
            },
            &self.counts,
        );
        self.new_starts.push(to_u32(next));
        self.row += 1;
        self.row_start = next;
        self.fields = 1;
        self.pos = next;
    }

    fn summary(&self) -> Summary {
        Summary {
            scanned: self.pos,
            field_count_mode: self.counts.mode(),
            unterminated_quote: self.unterminated_quote,
        }
    }
}

/// Scans `bytes` into `index`, one chunk at a time. Chunk boundaries are
/// fixed (every `chunk_bytes` after the BOM, rounded up to whole units), so
/// a chunk that ends a unit late doesn't move the later ones.
pub(crate) fn run<U: Units, O: RowObserver>(
    index: &RowIndex,
    bytes: &[u8],
    cancel: &AtomicBool,
    chunk_bytes: usize,
    mut on_progress: impl FnMut(Progress),
    observer: &mut O,
) -> Result<(), IndexError> {
    let dialect = index.dialect;
    let len = bytes.len();
    let step = chunk_bytes.max(1).next_multiple_of(U::WIDTH);
    let view = View::<U>::new(bytes, 0, dialect);
    let mut scanner = Scanner::<U>::new(len, dialect);
    index.begin(len);
    let mut boundary = dialect.bom_len;
    loop {
        // The flag carries no data with it, so `Relaxed` is enough: we only
        // need to see the store eventually, and the next chunk will.
        if cancel.load(Ordering::Relaxed) {
            return Err(IndexError::Cancelled);
        }
        boundary = boundary.saturating_add(step).min(len);
        scanner.scan(&view, boundary, observer);
        let done = boundary == len;
        if done {
            scanner.finish(observer);
        }
        let summary = scanner.summary();
        let progress = index.publish(&mut scanner.new_starts, &summary, done);
        observer.published(progress.rows, done);
        on_progress(progress);
        if done {
            return Ok(());
        }
    }
}

/// The scan of a file that arrives in chunks, in order ([`Source::stream`]
/// on a removable drive, ADR-0006). Each chunk is scanned as far as it can
/// be without the next one, which is all of it but its last unit: a CR
/// there may be half of a CRLF, and a quote half of a `""`. Those few
/// bytes, and the unit before the scanner's position, are kept for the
/// next chunk; nothing else is.
///
/// [`Source::stream`]: crate::source::Source::stream
pub(crate) struct Chunked<U> {
    scanner: Scanner<U>,
    dialect: IndexDialect,
    /// The bytes kept from earlier chunks, then the chunk being scanned.
    /// They start at `window_base` in the file.
    window: Vec<u8>,
    window_base: usize,
}

impl<U: Units> Chunked<U> {
    /// Starts a scan of a `len`-byte file into `index`.
    pub(crate) fn new(index: &RowIndex, len: usize) -> Self {
        index.begin(len);
        let dialect = index.dialect;
        Chunked {
            scanner: Scanner::new(len, dialect),
            dialect,
            window: Vec::new(),
            window_base: 0,
        }
    }

    /// How many bytes have arrived so far.
    pub(crate) fn received(&self) -> usize {
        self.window_base + self.window.len()
    }

    /// Scans the next chunk, which follows the last one in the file, and
    /// publishes the rows it finished. The caller has checked that the
    /// chunk fits in the file.
    pub(crate) fn push<O: RowObserver>(
        &mut self,
        index: &RowIndex,
        chunk: &[u8],
        observer: &mut O,
    ) -> Progress {
        let w = U::WIDTH;
        self.window.extend_from_slice(chunk);
        let end = self.received();
        // Leave the last few bytes for the next chunk (the observer's
        // `LOOKAHEAD`, and at least a unit), and stop on a unit boundary
        // (units start at the end of the BOM).
        let bom_len = self.dialect.bom_len;
        let to = end
            .saturating_sub(O::LOOKAHEAD.max(w))
            .checked_sub(bom_len)
            .map(|past| bom_len + past - past % w);
        if let Some(to) = to.filter(|&to| to > self.scanner.pos) {
            let view = View::<U>::new(&self.window, self.window_base, self.dialect);
            self.scanner.scan(&view, to, observer);
        }
        // Keep the unit before the scanner's position and everything after
        // it: at most a few units, since the scan got to within
        // `O::LOOKAHEAD` of the end.
        let keep_from = self
            .scanner
            .pos
            .saturating_sub(w)
            .clamp(self.window_base, end);
        self.window.drain(..keep_from - self.window_base);
        self.window_base = keep_from;
        let summary = self.scanner.summary();
        let progress = index.publish(&mut self.scanner.new_starts, &summary, false);
        observer.published(progress.rows, false);
        progress
    }

    /// Scans what is left once every chunk has arrived, ends the last row
    /// and marks the index complete.
    pub(crate) fn finish<O: RowObserver>(mut self, index: &RowIndex, observer: &mut O) -> Progress {
        let len = self.scanner.len;
        if len > self.scanner.pos {
            let view = View::<U>::new(&self.window, self.window_base, self.dialect);
            self.scanner.scan(&view, len, observer);
        }
        self.scanner.finish(observer);
        let summary = self.scanner.summary();
        let progress = index.publish(&mut self.scanner.new_starts, &summary, true);
        observer.published(progress.rows, true);
        progress
    }
}

// ---------------------------------------------------------------------------
// Reading line endings back

/// The line ending at the end of the row extent `start..next`, read from
/// `window`, the file's bytes from `base` on, which must hold the extent.
/// Every row but the last ends in one. The last row has one only if its
/// last unit is CR or LF: it can't end in CR or LF otherwise, except
/// inside an unterminated quote, which the caller handles.
pub(crate) fn line_ending_before(
    window: &[u8],
    base: usize,
    dialect: IndexDialect,
    start: usize,
    next: usize,
) -> Option<LineEnding> {
    match dialect.code_unit {
        CodeUnit::Byte => line_ending_in(&View::<Bytes>::new(window, base, dialect), start, next),
        CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
            // A final odd byte in UTF-16 isn't a whole unit, so a row that
            // ends with one has no line ending.
            if !(next - dialect.bom_len).is_multiple_of(2) {
                return None;
            }
            line_ending_in(&View::<Utf16>::new(window, base, dialect), start, next)
        }
    }
}

fn line_ending_in<U: Units>(view: &View<'_, U>, start: usize, next: usize) -> Option<LineEnding> {
    let w = U::WIDTH;
    let last = next.checked_sub(w).filter(|&p| p >= start)?;
    if view.is(last, LF) {
        // A CR straight before an LF is always part of one CRLF.
        let crlf = last
            .checked_sub(w)
            .is_some_and(|p| p >= start && view.is(p, CR));
        Some(if crlf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        })
    } else if view.is(last, CR) {
        Some(LineEnding::Cr)
    } else {
        None
    }
}

/// A line ending's length in bytes.
pub(crate) fn line_ending_len(line_ending: LineEnding, code_unit: CodeUnit) -> usize {
    let units = match line_ending {
        LineEnding::Lf | LineEnding::Cr => 1,
        LineEnding::Crlf => 2,
    };
    units * code_unit.width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_counts_track_the_mode_with_first_seen_ties() {
        let mut counts = FieldCounts::default();
        assert_eq!(counts.mode(), None);
        for (fields, mode) in [(3, 3), (2, 3), (2, 2), (3, 3), (5, 3), (5, 3), (5, 5)] {
            counts.add(fields);
            assert_eq!(counts.mode(), Some(mode), "after adding {fields}");
        }
    }

    #[test]
    fn utf16_units_ignore_bytes_in_the_wrong_half() {
        // U+0A22 and U+220A in both byte orders, then a real LF.
        for (big_endian, bytes) in [
            (false, b"\xFF\xFE\x22\x0A\x0A\x22\x0A\x00".as_slice()),
            (true, b"\xFE\xFF\x0A\x22\x22\x0A\x00\x0A".as_slice()),
        ] {
            let units = Utf16 {
                phase: 0,
                big_endian,
            };
            assert_eq!(units.find1(bytes, 2, bytes.len(), LF), Some(6));
            assert_eq!(
                units.find3(bytes, 2, bytes.len(), [b'"', CR, LF]),
                Some((6, LF))
            );
            assert_eq!(units.count(bytes, 2, bytes.len(), b'"'), 0);
            assert!(units.is(bytes, 6, LF));
            assert!(!units.is(bytes, 7, LF));
        }
    }
}
