//! Which rows have a warning or an error, for every row, not only the first
//! [`MAX_LOCATIONS`](super::MAX_LOCATIONS) of each kind: the gutter markers
//! of ADR-0002 question 7 (1.7).
//!
//! **Why one byte per row, not one bit.** A row is marked if it has a
//! field-level warning (text after a closing quote, invalid encoding, NUL),
//! the unterminated quote, or is ragged. The first four are known when the
//! row ends, so one bit would do. But whether a row is ragged depends on
//! the most common field count, which can change until the last row is
//! indexed, and a bit computed against the mode so far would be wrong for
//! earlier rows when it changes. So each row keeps its field count too, in
//! 7 bits, and "ragged" is worked out when asked, against the current mode.
//! That is right for every provisional mode while indexing, and for the
//! final one.
//!
//! Each row's byte ([`code`]) is:
//!
//! - bit 7: the row has a field-level warning or the unterminated quote;
//! - bits 0–6: 0 for a blank line (never ragged), otherwise the field
//!   count, or [`WIDE`] for 127 fields or more, whose exact count is in a
//!   side list. A row with 127 fields has at least 126 bytes, so that list
//!   holds at most one 8-byte entry per 126 bytes of the file, and in
//!   practice only rows of very wide files.
//!
//! That is 1 MB per million rows, a quarter of the row index's 4 MB.

use std::ops::Range;

/// Bit 7 of a row's code: a field-level warning or the unterminated quote.
const FLAG: u8 = 0x80;

/// The field-count code for 127 fields or more.
pub(crate) const WIDE: u8 = 0x7F;

/// Which marked rows a search looks for (task 1.7): any, for the gutter;
/// ragged or flagged ones, for each kind's **Previous** and **Next** in the
/// details popover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mark {
    /// Any warning or error: ragged, or flagged.
    Any,
    /// Ragged rows only: a field count other than the mode's.
    Ragged,
    /// Rows with a field-level warning or the unterminated quote (bit 7).
    Flagged,
}

/// A row's code. `fields` is the row's field count (ignored for a blank
/// line). For [`WIDE`] rows, the caller also records the exact count.
pub(crate) fn code(fields: usize, blank: bool, flagged: bool) -> u8 {
    let count = if blank {
        0
    } else {
        u8::try_from(fields).map_or(WIDE, |n| n.min(WIDE))
    };
    count | if flagged { FLAG } else { 0 }
}

/// A row's code, read ([`RowMarks::code_of`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowCode {
    /// Its field count, or `None` for a blank line.
    pub(crate) fields: Option<usize>,
    /// It has a field-level warning or the unterminated quote.
    pub(crate) flagged: bool,
}

impl RowCode {
    /// `code`, with a [`WIDE`] row's exact field count `wide`.
    fn read(code: u8, wide: Option<usize>) -> RowCode {
        let fields = match code & !FLAG {
            0 => None,
            WIDE => wide,
            n => Some(usize::from(n)),
        };
        RowCode {
            fields,
            flagged: code & FLAG != 0,
        }
    }
}

/// Every row's code, and the field counts of wide rows.
#[derive(Debug, Default)]
pub(crate) struct RowMarks {
    codes: Vec<u8>,
    /// `(row, field count − 1)` of every [`WIDE`] row, in row order. Both
    /// fit in a `u32`, since neither can be more than the file's length.
    wide: Vec<(u32, u32)>,
    /// The most common field count among the rows so far.
    mode: Option<usize>,
}

/// Copies of a [`RowMarks`]' lists with the room the next rows need, made
/// before the write lock is taken ([`RowMarks::room`]); or the old lists
/// [`RowMarks::extend`] gives back.
#[derive(Debug, Default)]
pub(crate) struct MarksRoom {
    codes: Option<Vec<u8>>,
    wide: Option<Vec<(u32, u32)>>,
}

impl RowMarks {
    /// The copies the lists need to take `codes` more codes and `wide`
    /// more wide rows ([`growth::room`](crate::growth::room)), made by the
    /// caller under the read lock, before it takes the write lock for
    /// [`extend`](Self::extend).
    pub(crate) fn room(&self, codes: usize, wide: usize, done: bool) -> MarksRoom {
        MarksRoom {
            codes: crate::growth::room(&self.codes, self.codes.capacity(), codes, done),
            wide: crate::growth::room(&self.wide, self.wide.capacity(), wide, done),
        }
    }

    /// Adds the next rows' codes and wide rows (emptying both), and the
    /// mode as of the last of them, into `room` made beforehand
    /// ([`room`](Self::room); an empty one grows the lists in place).
    /// `done` if those are the file's last rows: the lists then give back
    /// the room they grew into, as the row index's offsets do, since no
    /// more rows will come. Returns the old lists, for the caller to drop
    /// once it has released its lock.
    pub(crate) fn extend(
        &mut self,
        room: MarksRoom,
        codes: &mut Vec<u8>,
        wide: &mut Vec<(u32, u32)>,
        mode: Option<usize>,
        done: bool,
    ) -> MarksRoom {
        let old = MarksRoom {
            codes: crate::growth::swap_in(&mut self.codes, room.codes),
            wide: crate::growth::swap_in(&mut self.wide, room.wide),
        };
        self.codes.append(codes);
        self.wide.append(wide);
        self.mode = mode;
        if done {
            // Only if an exact copy couldn't go in: a no-op otherwise.
            self.codes.shrink_to_fit();
            self.wide.shrink_to_fit();
        }
        old
    }

    /// Row `row`'s code, read, or `None` if it isn't indexed yet.
    pub(crate) fn code_of(&self, row: usize) -> Option<RowCode> {
        let &code = self.codes.get(row)?;
        let wide = if code & !FLAG == WIDE {
            self.wide_fields(row)
        } else {
            None
        };
        Some(RowCode::read(code, wide))
    }

    /// Hands each of rows `rows`' codes to `each`, in order (those indexed).
    pub(crate) fn for_each_code(&self, rows: Range<usize>, each: &mut dyn FnMut(usize, RowCode)) {
        let end = rows.end.min(self.codes.len());
        let Some(codes) = self.codes.get(rows.start..end) else {
            return;
        };
        let mut w = self.wide.partition_point(|&(r, _)| row_of(r) < rows.start);
        for (row, &code) in (rows.start..).zip(codes) {
            let wide = self.step_wide(code, &mut w, row);
            each(row, RowCode::read(code, wide));
        }
    }

    /// Up to `max` rows `pick` picks by their codes: from `at` on, in
    /// order, if `forward`, else before `at`, nearest first. For marks that
    /// depend on more than the code: a row's field count after column
    /// inserts and deletes (task 2.4b). One step per row.
    pub(crate) fn rows_picked(
        &self,
        at: usize,
        forward: bool,
        max: usize,
        pick: &dyn Fn(RowCode) -> bool,
    ) -> Vec<(usize, RowCode)> {
        let mut rows = Vec::new();
        if forward {
            let Some(tail) = self.codes.get(at..) else {
                return rows;
            };
            let mut w = self.wide.partition_point(|&(r, _)| row_of(r) < at);
            for (row, &code) in (at..).zip(tail) {
                let wide = self.step_wide(code, &mut w, row);
                let read = RowCode::read(code, wide);
                if pick(read) {
                    rows.push((row, read));
                    if rows.len() >= max {
                        break;
                    }
                }
            }
        } else {
            let head = &self.codes[..at.min(self.codes.len())];
            let mut w = self.wide.partition_point(|&(r, _)| row_of(r) < head.len());
            for (row, &code) in head.iter().enumerate().rev() {
                let wide = if code & !FLAG == WIDE {
                    w = w.saturating_sub(1);
                    self.wide_entry(w, row)
                } else {
                    None
                };
                let read = RowCode::read(code, wide);
                if pick(read) {
                    rows.push((row, read));
                    if rows.len() >= max {
                        break;
                    }
                }
            }
        }
        rows
    }

    /// How many bytes the lists hold room for, for tests.
    #[cfg(test)]
    pub(crate) fn capacity_bytes(&self) -> usize {
        self.codes.capacity() + self.wide.capacity() * std::mem::size_of::<(u32, u32)>()
    }

    /// How many rows have a code: the rows indexed so far.
    pub(crate) fn len(&self) -> usize {
        self.codes.len()
    }

    /// The most common field count among the rows so far.
    pub(crate) fn mode(&self) -> Option<usize> {
        self.mode
    }

    /// True if `row` has a warning or an error. False for a row that isn't
    /// indexed yet.
    pub(crate) fn has(&self, row: usize) -> bool {
        self.is(row, Mark::Any)
    }

    /// True if `row` is marked as `which` says. False for a row that isn't
    /// indexed yet.
    pub(crate) fn is(&self, row: usize, which: Mark) -> bool {
        self.codes
            .get(row)
            .is_some_and(|&code| self.marked(row, code, which))
    }

    /// The first marked row at or after `from`.
    pub(crate) fn next(&self, from: usize) -> Option<usize> {
        self.next_where(from, Mark::Any)
    }

    /// The last marked row before `to`.
    pub(crate) fn previous(&self, to: usize) -> Option<usize> {
        self.previous_where(to, Mark::Any)
    }

    /// The first row at or after `from` marked as `which` says.
    pub(crate) fn next_where(&self, from: usize, which: Mark) -> Option<usize> {
        let tail = self.codes.get(from..)?;
        match self.test(which) {
            Test::Byte(test) => {
                // Skip blocks with no marked row, 64 codes at a time: a loop
                // without an early exit, which the compiler vectorises.
                let mut at = from;
                for block in tail.chunks(BLOCK) {
                    if block
                        .iter()
                        .fold(false, |any, &code| any | test.marked(code))
                    {
                        let i = block.iter().position(|&code| test.marked(code))?;
                        return Some(at + i);
                    }
                    at += block.len();
                }
                None
            }
            // One walk for each `which`, so it is decided once per search,
            // not again on every row (see `WideTest`).
            Test::Wide(test) => match which {
                Mark::Any => self.next_wide::<true, true>(from, tail, test),
                Mark::Ragged => self.next_wide::<false, true>(from, tail, test),
                Mark::Flagged => self.next_wide::<true, false>(from, tail, test),
            },
        }
    }

    /// [`next_where`](Self::next_where) in a wide mode, for flagged rows if
    /// `FLAGGED` and ragged ones if `RAGGED`: the first such row of `tail`,
    /// which starts at row `from`. Wide rows need their exact count from
    /// the side list. It is walked alongside the codes (one search to find
    /// where to start), so the cost stays one step per row, as for a narrow
    /// file, not a search per row.
    fn next_wide<const FLAGGED: bool, const RAGGED: bool>(
        &self,
        from: usize,
        tail: &[u8],
        test: WideTest,
    ) -> Option<usize> {
        let mut w = self.wide.partition_point(|&(r, _)| row_of(r) < from);
        for (row, &code) in (from..).zip(tail) {
            let fields = self.step_wide(code, &mut w, row);
            if (FLAGGED && code & FLAG != 0) || (RAGGED && test.ragged(code, fields)) {
                return Some(row);
            }
        }
        None
    }

    /// The last row before `to` marked as `which` says.
    pub(crate) fn previous_where(&self, to: usize, which: Mark) -> Option<usize> {
        let head = &self.codes[..to.min(self.codes.len())];
        match self.test(which) {
            Test::Byte(test) => {
                let mut end = head.len();
                for block in head.rchunks(BLOCK) {
                    end -= block.len();
                    if block
                        .iter()
                        .fold(false, |any, &code| any | test.marked(code))
                    {
                        let i = block.iter().rposition(|&code| test.marked(code))?;
                        return Some(end + i);
                    }
                }
                None
            }
            // As in `next_where`.
            Test::Wide(test) => match which {
                Mark::Any => self.previous_wide::<true, true>(head, test),
                Mark::Ragged => self.previous_wide::<false, true>(head, test),
                Mark::Flagged => self.previous_wide::<true, false>(head, test),
            },
        }
    }

    /// [`next_wide`](Self::next_wide) backwards: the last such row of
    /// `head`, which starts at row 0. `w` is one past the last wide row in
    /// `head`, and walks the side list backwards.
    fn previous_wide<const FLAGGED: bool, const RAGGED: bool>(
        &self,
        head: &[u8],
        test: WideTest,
    ) -> Option<usize> {
        let mut w = self.wide.partition_point(|&(r, _)| row_of(r) < head.len());
        for (row, &code) in head.iter().enumerate().rev() {
            let fields = if code & !FLAG == WIDE {
                w = w.checked_sub(1)?;
                self.wide_entry(w, row)
            } else {
                None
            };
            if (FLAGGED && code & FLAG != 0) || (RAGGED && test.ragged(code, fields)) {
                return Some(row);
            }
        }
        None
    }

    /// True if the row with this code is marked as `which` says.
    fn marked(&self, row: usize, code: u8, which: Mark) -> bool {
        let fields = if code & !FLAG == WIDE {
            self.wide_fields(row)
        } else {
            None
        };
        self.marked_with(code, fields, which)
    }

    /// [`marked`](Self::marked), given a [`WIDE`] row's exact field count.
    fn marked_with(&self, code: u8, wide_fields: Option<usize>, which: Mark) -> bool {
        let flagged = code & FLAG != 0;
        match which {
            Mark::Flagged => return flagged,
            Mark::Any if flagged => return true,
            Mark::Any | Mark::Ragged => {}
        }
        let count = code & !FLAG;
        if count == 0 {
            return false; // a blank line
        }
        let Some(mode) = self.mode else {
            return false;
        };
        if count == WIDE {
            wide_fields != Some(mode)
        } else {
            usize::from(count) != mode
        }
    }

    /// For a forward walk: if `code` is a [`WIDE`] row's, the exact count
    /// from side-list entry `*w`, which is then moved on.
    fn step_wide(&self, code: u8, w: &mut usize, row: usize) -> Option<usize> {
        if code & !FLAG != WIDE {
            return None;
        }
        let fields = self.wide_entry(*w, row);
        *w += 1;
        fields
    }

    /// The field count in side-list entry `w`, which belongs to `row`.
    fn wide_entry(&self, w: usize, row: usize) -> Option<usize> {
        let &(r, delimiters) = self.wide.get(w)?;
        debug_assert_eq!(row_of(r), row, "the wide list is out of step");
        usize::try_from(delimiters).ok().map(|d| d + 1)
    }

    /// The field count of a [`WIDE`] row.
    fn wide_fields(&self, row: usize) -> Option<usize> {
        let row = u32::try_from(row).ok()?;
        let i = self.wide.binary_search_by_key(&row, |&(r, _)| r).ok()?;
        usize::try_from(self.wide[i].1)
            .ok()
            .map(|delimiters| delimiters + 1)
    }

    /// How a search for `which` tests each row: from the code alone when
    /// the mode isn't wide, or when only the flag matters; otherwise with
    /// wide rows' exact counts too.
    fn test(&self, which: Mark) -> Test {
        if which == Mark::Flagged {
            return Test::Byte(ByteTest { mode: None, which });
        }
        match self.mode {
            None => Test::Byte(ByteTest { mode: None, which }),
            Some(mode) => match u8::try_from(mode).ok().filter(|&m| m < WIDE) {
                Some(m) => Test::Byte(ByteTest {
                    mode: Some(m),
                    which,
                }),
                None => Test::Wide(WideTest { mode }),
            },
        }
    }
}

/// How a search tests each row ([`RowMarks::test`]).
enum Test {
    Byte(ByteTest),
    Wide(WideTest),
}

/// A row number from the side list (a `u32`, which always fits).
fn row_of(row: u32) -> usize {
    usize::try_from(row).unwrap_or(usize::MAX)
}

/// How many codes [`RowMarks::next`] and [`RowMarks::previous`] check at once.
const BLOCK: usize = 64;

/// [`RowMarks::marked`] when the mode is below [`WIDE`] (or there is none),
/// so every wide row is ragged and the code alone decides; or when only the
/// flag matters.
#[derive(Clone, Copy)]
struct ByteTest {
    mode: Option<u8>,
    which: Mark,
}

impl ByteTest {
    fn marked(self, code: u8) -> bool {
        let count = code & !FLAG;
        let flagged = code & FLAG != 0;
        let ragged = count != 0 && self.mode.is_some_and(|m| count != m);
        match self.which {
            Mark::Any => flagged | ragged,
            Mark::Ragged => ragged,
            Mark::Flagged => flagged,
        }
    }
}

/// Whether a row is ragged when the mode is [`WIDE`] or more: a wide row is
/// if its exact count isn't the mode, and every other row (all narrower)
/// is unless it is blank.
///
/// This is [`RowMarks::marked`] with what a wide mode already settles taken
/// out: that there is a mode, and how a narrower row compares with it. With
/// that, and `which` decided once per search ([`RowMarks::next_wide`]), a
/// walk over a wide file is a few instructions per row. Every step counts
/// at about half a nanosecond a row: deciding `which` on each row, as 1.7
/// first did, made **Previous** a third slower (docs/tasks/1.7.md, "The
/// marks benchmarks").
#[derive(Clone, Copy)]
struct WideTest {
    mode: usize,
}

impl WideTest {
    /// `wide_fields` is the exact count of a [`WIDE`] row, from the side
    /// list.
    fn ragged(self, code: u8, wide_fields: Option<usize>) -> bool {
        let count = code & !FLAG;
        if count == WIDE {
            wide_fields != Some(self.mode)
        } else {
            count != 0
        }
    }
}
