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

/// Bit 7 of a row's code: a field-level warning or the unterminated quote.
const FLAG: u8 = 0x80;

/// The field-count code for 127 fields or more.
pub(crate) const WIDE: u8 = 0x7F;

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

impl RowMarks {
    /// Adds the next rows' codes and wide rows (emptying both), and the
    /// mode as of the last of them.
    pub(crate) fn extend(
        &mut self,
        codes: &mut Vec<u8>,
        wide: &mut Vec<(u32, u32)>,
        mode: Option<usize>,
    ) {
        self.codes.append(codes);
        self.wide.append(wide);
        self.mode = mode;
    }

    /// True if `row` has a warning or an error. False for a row that isn't
    /// indexed yet.
    pub(crate) fn has(&self, row: usize) -> bool {
        self.codes
            .get(row)
            .is_some_and(|&code| self.marked(row, code))
    }

    /// The first marked row at or after `from`.
    pub(crate) fn next(&self, from: usize) -> Option<usize> {
        let tail = self.codes.get(from..)?;
        match self.byte_test() {
            Some(test) => {
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
            None => (from..self.codes.len()).find(|&row| self.marked(row, self.codes[row])),
        }
    }

    /// The last marked row before `to`.
    pub(crate) fn previous(&self, to: usize) -> Option<usize> {
        let head = &self.codes[..to.min(self.codes.len())];
        match self.byte_test() {
            Some(test) => {
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
            None => (0..head.len())
                .rev()
                .find(|&row| self.marked(row, self.codes[row])),
        }
    }

    /// True if the row with this code has a warning or an error.
    fn marked(&self, row: usize, code: u8) -> bool {
        if code & FLAG != 0 {
            return true;
        }
        let count = code & !FLAG;
        if count == 0 {
            return false; // a blank line
        }
        let Some(mode) = self.mode else {
            return false;
        };
        if count == WIDE {
            self.wide_fields(row) != Some(mode)
        } else {
            usize::from(count) != mode
        }
    }

    /// The field count of a [`WIDE`] row.
    fn wide_fields(&self, row: usize) -> Option<usize> {
        let row = u32::try_from(row).ok()?;
        let i = self.wide.binary_search_by_key(&row, |&(r, _)| r).ok()?;
        usize::try_from(self.wide[i].1)
            .ok()
            .map(|delimiters| delimiters + 1)
    }

    /// A test that needs only the code, when the mode isn't wide.
    fn byte_test(&self) -> Option<ByteTest> {
        match self.mode {
            None => Some(ByteTest { mode: None }),
            Some(mode) => u8::try_from(mode)
                .ok()
                .filter(|&m| m < WIDE)
                .map(|m| ByteTest { mode: Some(m) }),
        }
    }
}

/// How many codes [`RowMarks::next`] and [`RowMarks::previous`] check at once.
const BLOCK: usize = 64;

/// [`RowMarks::marked`] when the mode is below [`WIDE`] (or there is none),
/// so every wide row is ragged and the code alone decides.
#[derive(Clone, Copy)]
struct ByteTest {
    mode: Option<u8>,
}

impl ByteTest {
    fn marked(self, code: u8) -> bool {
        let count = code & !FLAG;
        code & FLAG != 0 || (count != 0 && self.mode.is_some_and(|m| count != m))
    }
}
