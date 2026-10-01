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

use std::collections::HashMap;
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
    /// Where code units start: the end of the BOM.
    base: usize,
    big_endian: bool,
}

impl Utf16 {
    pub(crate) fn new(dialect: IndexDialect) -> Self {
        Utf16 {
            base: dialect.bom_len,
            big_endian: dialect.code_unit == CodeUnit::Utf16Be,
        }
    }

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
            let in_unit = (hit - self.base) % 2;
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

/// Told about every row as the scanner finds it.
///
/// HOOK(1.5): diagnostics are collected in this same pass (DESIGN §3.3).
/// Ragged rows, blank lines and mixed line endings need exactly these facts.
/// Field-level kinds (text after a closing quote, invalid encoding, NUL)
/// need more; the places in [`Scanner::scan`] marked `HOOK(1.5)` are where
/// the scanner learns about quotes. Tests use an observer to read each
/// row's field count.
pub(crate) trait RowObserver {
    fn row(&mut self, facts: &RowFacts);
}

/// Ignores every row. The compiler removes the calls entirely.
pub(crate) struct NoObserver;

impl RowObserver for NoObserver {
    fn row(&mut self, _facts: &RowFacts) {}
}

// ---------------------------------------------------------------------------
// Field counts

/// Counts how many rows have each field count, to find the most common one
/// (ties go to the count seen first). Most files have one or two distinct
/// counts, and most rows have the same count as the row before, so that
/// case skips the hash map.
#[derive(Default)]
struct FieldCounts {
    /// `(field count, rows)`, in the order each count was first seen.
    seen: Vec<(usize, u64)>,
    /// Where each field count is in `seen`.
    position: HashMap<usize, usize>,
    /// The position of the last count added.
    last: Option<usize>,
    /// The position of the most common count so far.
    best: Option<usize>,
}

impl FieldCounts {
    fn add(&mut self, fields: usize) {
        let i = match self.last {
            Some(i) if self.seen[i].0 == fields => i,
            _ => *self.position.entry(fields).or_insert_with(|| {
                self.seen.push((fields, 0));
                self.seen.len() - 1
            }),
        };
        self.seen[i].1 += 1;
        self.last = Some(i);
        // Only count `i` changed, so it is the only one that can overtake
        // the best. An equal count wins only if it was seen first.
        let n = self.seen[i].1;
        self.best = match self.best {
            Some(b) if self.seen[b].1 > n || (self.seen[b].1 == n && b <= i) => Some(b),
            _ => Some(i),
        };
    }

    fn mode(&self) -> Option<usize> {
        self.best.map(|b| self.seen[b].0)
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

struct Scanner<'a, U> {
    bytes: &'a [u8],
    units: U,
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
}

impl<'a, U: Units> Scanner<'a, U> {
    fn new(bytes: &'a [u8], units: U, dialect: IndexDialect) -> Self {
        Scanner {
            bytes,
            units,
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
        }
    }

    /// Scans until `pos` reaches `to`. It can stop a unit or two past `to`,
    /// when it looks ahead for the LF of a CRLF or the second quote of `""`;
    /// the next chunk carries on from there.
    fn scan<O: RowObserver>(&mut self, to: usize, observer: &mut O) {
        let w = U::WIDTH;
        let bytes = self.bytes;
        while self.pos < to {
            if self.open_quote.is_some() {
                // Inside a quoted field: only a quote matters.
                let Some(q) = self.units.find1(bytes, self.pos, to, self.quote) else {
                    self.pos = to;
                    break;
                };
                if self.units.is(bytes, q + w, self.quote) {
                    // `""`: an escaped quote.
                    self.pos = q + 2 * w;
                } else {
                    // The closing quote. HOOK(1.5): if the unit after it
                    // isn't a delimiter, CR, LF or the end of the file, the
                    // field has text after its closing quote, starting at
                    // `q + w`. `open_quote` is the field's start.
                    self.open_quote = None;
                    self.pos = q + w;
                }
                continue;
            }

            // Outside quotes: the next quote, CR or LF.
            let found = self.units.find3(bytes, self.pos, to, [self.quote, CR, LF]);
            let end = found.map_or(to, |(at, _)| at);
            self.fields += self.units.count(bytes, self.pos, end, self.delimiter);
            let Some((at, value)) = found else {
                self.pos = to;
                break;
            };
            if value == LF {
                self.end_row(at, Some(LineEnding::Lf), at + w, observer);
            } else if value == CR {
                if self.units.is(bytes, at + w, LF) {
                    self.end_row(at, Some(LineEnding::Crlf), at + 2 * w, observer);
                } else {
                    self.end_row(at, Some(LineEnding::Cr), at + w, observer);
                }
            } else {
                // A quote opens a field only as its first unit.
                if at == self.row_start || self.units.is(bytes, at - w, self.delimiter) {
                    self.open_quote = Some(at);
                }
                // Otherwise it is literal: in an unquoted field (`a"b`), or
                // in text after a closing quote (ADR-0003 decision 3).
                self.pos = at + w;
            }
        }
    }

    /// The end of the file: the last row, if it has no line ending.
    fn finish<O: RowObserver>(&mut self, observer: &mut O) {
        let len = self.bytes.len();
        if let Some(open) = self.open_quote.take() {
            // HOOK(1.5): the unterminated quote diagnostic.
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
        observer.row(&RowFacts {
            row: self.row,
            span: self.row_start..content_end,
            line_ending,
            next_start: next,
            fields: self.fields,
        });
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
    units: U,
    cancel: &AtomicBool,
    chunk_bytes: usize,
    mut on_progress: impl FnMut(Progress),
    observer: &mut O,
) -> Result<(), IndexError> {
    let dialect = index.dialect;
    let len = bytes.len();
    let step = chunk_bytes.max(1).next_multiple_of(U::WIDTH);
    let mut scanner = Scanner::new(bytes, units, dialect);
    index.begin(len);
    let mut boundary = dialect.bom_len;
    loop {
        // The flag carries no data with it, so `Relaxed` is enough: we only
        // need to see the store eventually, and the next chunk will.
        if cancel.load(Ordering::Relaxed) {
            return Err(IndexError::Cancelled);
        }
        boundary = boundary.saturating_add(step).min(len);
        scanner.scan(boundary, observer);
        let done = boundary == len;
        if done {
            scanner.finish(observer);
        }
        let summary = scanner.summary();
        let progress = index.publish(&mut scanner.new_starts, &summary, done);
        on_progress(progress);
        if done {
            return Ok(());
        }
    }
}

// ---------------------------------------------------------------------------
// Reading line endings back

/// The line ending at the end of the row extent `start..next`, read from
/// the bytes. Every row but the last ends in one. The last row has one only
/// if its last unit is CR or LF: it can't end in CR or LF otherwise, except
/// inside an unterminated quote, which the caller handles.
pub(crate) fn line_ending_before(
    bytes: &[u8],
    dialect: IndexDialect,
    start: usize,
    next: usize,
) -> Option<LineEnding> {
    match dialect.code_unit {
        CodeUnit::Byte => line_ending_in(&Bytes, bytes, start, next),
        CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
            // A final odd byte in UTF-16 isn't a whole unit, so a row that
            // ends with one has no line ending.
            if !(next - dialect.bom_len).is_multiple_of(2) {
                return None;
            }
            line_ending_in(&Utf16::new(dialect), bytes, start, next)
        }
    }
}

fn line_ending_in<U: Units>(
    units: &U,
    bytes: &[u8],
    start: usize,
    next: usize,
) -> Option<LineEnding> {
    let w = U::WIDTH;
    let last = next.checked_sub(w).filter(|&p| p >= start)?;
    if units.is(bytes, last, LF) {
        // A CR straight before an LF is always part of one CRLF.
        let crlf = last
            .checked_sub(w)
            .is_some_and(|p| p >= start && units.is(bytes, p, CR));
        Some(if crlf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        })
    } else if units.is(bytes, last, CR) {
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
                base: 2,
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
