//! A small quote-aware row scanner for detection samples.
//!
//! It follows the rules of DESIGN §3.4 and ADR-0003: a quote opens a quoted
//! field only as the field's first character; inside quotes `""` is one
//! quote; text after a closing quote is literal up to the next delimiter or
//! line end; CR LF is one line ending; a line ending at the end of the text
//! ends the last row without starting another; and a quote that never
//! closes runs to the end of the text, so that row has no line ending.
//!
//! The row index (task 1.3) is the real parser. This one only has to be
//! right about field counts and line endings in a sample.

use std::ops::Range;

use super::units::{Units, decode};
use crate::dialect::{LineEnding, QUOTE};

const CR: u16 = b'\r' as u16;
const LF: u16 = b'\n' as u16;
const QUOTE_UNIT: u16 = QUOTE as u16;

/// One row of a sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    /// The row's units, without its line ending.
    pub span: Range<usize>,
    /// The number of fields (at least 1).
    pub fields: usize,
    /// The line ending, or `None` if the text ended first.
    pub ending: Option<LineEnding>,
    /// The row's quotes aren't all well formed: a quote in the middle of an
    /// unquoted field, text after a closing quote, or a quote that never
    /// closes. Under the right delimiter a tidy file has none.
    pub irregular: bool,
}

impl Row {
    /// A row with no units before its line ending (ADR-0003 decision 5).
    pub(crate) fn is_blank(&self) -> bool {
        self.span.is_empty() && self.ending.is_some()
    }
}

/// Where the scanner is within the current field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// At the start of a field: a quote here opens a quoted field.
    FieldStart,
    /// In an unquoted field.
    Unquoted,
    /// Inside quotes.
    Quoted,
    /// Inside quotes, just after a `"`: either the first half of `""` or
    /// the closing quote, depending on the next unit.
    QuoteInQuoted,
    /// After a closing quote (any text here is literal).
    AfterQuote,
}

/// A row the [`Scanner`] has finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ended {
    /// The row's length in units, without its line ending.
    pub len: usize,
    /// The number of fields (at least 1).
    pub fields: usize,
    /// The line ending, or `None` at the end of the text.
    pub ending: Option<LineEnding>,
    /// See [`Row::irregular`].
    pub irregular: bool,
}

impl Ended {
    /// A row with no units before its line ending.
    pub(crate) fn is_blank(&self) -> bool {
        self.len == 0 && self.ending.is_some()
    }
}

/// The row scanner, fed one unit at a time, so the same code reads a 64 KB
/// window and streams a whole file in chunks. It starts outside quotes.
#[derive(Clone, Debug)]
pub(crate) struct Scanner {
    delimiter: u16,
    state: State,
    /// The last unit was a CR outside quotes: an LF next makes it CR LF.
    pending_cr: bool,
    fields: usize,
    len: usize,
    irregular: bool,
}

impl Scanner {
    pub(crate) fn new(delimiter: u8) -> Self {
        Scanner {
            delimiter: u16::from(delimiter),
            state: State::FieldStart,
            pending_cr: false,
            fields: 1,
            len: 0,
            irregular: false,
        }
    }

    /// Reads the next unit. Returns the row it finished, if it finished one.
    pub(crate) fn feed(&mut self, u: u16) -> Option<Ended> {
        if self.pending_cr {
            self.pending_cr = false;
            if u == LF {
                return Some(self.end(Some(LineEnding::Crlf)));
            }
            let row = self.end(Some(LineEnding::Cr));
            // `u` starts the next row. It can't finish that row: an LF was
            // handled above, and a CR is only pending.
            let _ = self.step(u);
            return Some(row);
        }
        self.step(u)
    }

    /// The end of the text: the last row, if it has any units or a pending
    /// CR. A quote still open makes that row irregular.
    pub(crate) fn finish(&mut self) -> Option<Ended> {
        if self.pending_cr {
            self.pending_cr = false;
            return Some(self.end(Some(LineEnding::Cr)));
        }
        if self.len == 0 {
            return None;
        }
        self.irregular |= self.state == State::Quoted;
        Some(self.end(None))
    }

    fn step(&mut self, u: u16) -> Option<Ended> {
        match self.state {
            State::Quoted => {
                if u == QUOTE_UNIT {
                    self.state = State::QuoteInQuoted;
                }
                self.len += 1;
                None
            }
            State::QuoteInQuoted if u == QUOTE_UNIT => {
                self.state = State::Quoted; // `""`, an escaped quote
                self.len += 1;
                None
            }
            State::QuoteInQuoted => {
                self.state = State::AfterQuote; // the quote closed the field
                self.outside(u)
            }
            _ => self.outside(u),
        }
    }

    /// A unit outside quotes.
    fn outside(&mut self, u: u16) -> Option<Ended> {
        if u == LF {
            return Some(self.end(Some(LineEnding::Lf)));
        }
        if u == CR {
            self.pending_cr = true;
            return None;
        }
        self.len += 1;
        if u == self.delimiter {
            self.fields += 1;
            self.state = State::FieldStart;
            return None;
        }
        match self.state {
            State::FieldStart if u == QUOTE_UNIT => self.state = State::Quoted,
            State::FieldStart => self.state = State::Unquoted,
            // A quote in the middle of an unquoted field is literal.
            State::Unquoted => self.irregular |= u == QUOTE_UNIT,
            // Text after a closing quote.
            _ => self.irregular = true,
        }
        None
    }

    fn end(&mut self, ending: Option<LineEnding>) -> Ended {
        let row = Ended {
            len: self.len,
            fields: self.fields,
            ending,
            irregular: self.irregular,
        };
        self.state = State::FieldStart;
        self.fields = 1;
        self.len = 0;
        self.irregular = false;
        row
    }
}

/// The number of units a line ending takes.
const fn width(ending: Option<LineEnding>) -> usize {
    match ending {
        None => 0,
        Some(LineEnding::Crlf) => 2,
        Some(_) => 1,
    }
}

/// The rows of `units`, read with `delimiter`, starting outside quotes.
pub(crate) struct Rows<'a> {
    units: Units<'a>,
    scanner: Scanner,
    pos: usize,
    row_start: usize,
}

impl<'a> Rows<'a> {
    pub(crate) fn new(units: Units<'a>, delimiter: u8) -> Self {
        Rows {
            units,
            scanner: Scanner::new(delimiter),
            pos: 0,
            row_start: 0,
        }
    }

    fn row(&mut self, ended: Ended) -> Row {
        let start = self.row_start;
        self.row_start = start + ended.len + width(ended.ending);
        Row {
            span: start..start + ended.len,
            fields: ended.fields,
            ending: ended.ending,
            irregular: ended.irregular,
        }
    }
}

impl Iterator for Rows<'_> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        while self.pos < self.units.len() {
            let u = self.units.get(self.pos);
            self.pos += 1;
            if let Some(ended) = self.scanner.feed(u) {
                return Some(self.row(ended));
            }
        }
        let ended = self.scanner.finish()?;
        Some(self.row(ended))
    }
}

/// The rows of a window that are known to be whole. If the window was cut
/// from a longer file (`cut`), its last row may be missing its end: a row
/// with no line ending, or one ending in a CR at the very end (which could
/// be the first half of a CR LF), is dropped. If it is the only row (a row
/// longer than the window), it is kept, without that line ending, since
/// it is the only evidence there is.
pub(crate) fn whole_rows(units: Units<'_>, delimiter: u8, cut: bool) -> Vec<Row> {
    let len = units.len();
    let mut rows: Vec<Row> = Rows::new(units, delimiter).collect();
    let only_one = rows.len() == 1;
    if cut && let Some(last) = rows.last_mut() {
        let ends_in_cr = last.ending == Some(LineEnding::Cr) && last.span.end + 1 == len;
        if last.ending.is_none() || ends_in_cr {
            if only_one {
                last.ending = None;
            } else {
                rows.pop();
            }
        }
    }
    rows
}

/// The display values of the fields in `row` (DESIGN §3.4): a quoted
/// field's value without its quotes and with `""` as `"`, or its raw text
/// if there is text after its closing quote (ADR-0003 decision 2).
pub(crate) fn field_values(units: Units<'_>, row: &Row, delimiter: u8) -> Vec<String> {
    let delimiter = u16::from(delimiter);
    let end = row.span.end;
    let mut values = Vec::with_capacity(row.fields);
    let mut i = row.span.start;
    loop {
        let start = i;
        if i < end && units.get(i) == QUOTE_UNIT {
            // Quoted: collect the unescaped value, then any trailing text.
            let mut value: Vec<u8> = Vec::new();
            let mut closed = false;
            i += 1;
            let mut piece = i;
            while i < end {
                if units.get(i) == QUOTE_UNIT {
                    value.extend_from_slice(units.bytes(piece..i));
                    if i + 1 < end && units.get(i + 1) == QUOTE_UNIT {
                        value.extend_from_slice(units.bytes(i..i + 1));
                        i += 2;
                        piece = i;
                        continue;
                    }
                    closed = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !closed {
                value.extend_from_slice(units.bytes(piece..i));
            }
            let trailing_start = i;
            while i < end && units.get(i) != delimiter {
                i += 1;
            }
            if i > trailing_start {
                values.push(units.text(start..i)); // raw, ADR-0003
            } else {
                values.push(decode(&value, units.encoding()));
            }
        } else {
            while i < end && units.get(i) != delimiter {
                i += 1;
            }
            values.push(units.text(start..i));
        }
        if i >= end {
            return values;
        }
        i += 1; // the delimiter
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Encoding;

    fn rows(text: &[u8], delimiter: u8) -> Vec<(usize, Option<LineEnding>)> {
        Rows::new(Units::new(text, Encoding::Utf8), delimiter)
            .map(|r| (r.fields, r.ending))
            .collect()
    }

    fn irregular(text: &[u8]) -> Vec<bool> {
        Rows::new(Units::new(text, Encoding::Utf8), b',')
            .map(|r| r.irregular)
            .collect()
    }

    #[test]
    fn badly_formed_quotes_make_a_row_irregular() {
        assert_eq!(
            irregular(b"a,\"b\"\n\"\"\"x\"\"\",\"\"\nplain\n"),
            [false, false, false]
        );
        assert_eq!(irregular(b"a\"b\n\"a\"b\n\"a\" \n"), [true, true, true]);
        assert_eq!(irregular(b"ok\n\"open"), [false, true]);
    }

    use LineEnding::{Cr, Crlf, Lf};

    #[test]
    fn rows_end_at_each_kind_of_line_ending() {
        assert_eq!(
            rows(b"a,b\nc\r\nd,e,f\rg", b','),
            [(2, Some(Lf)), (1, Some(Crlf)), (3, Some(Cr)), (1, None)]
        );
        assert_eq!(rows(b"", b','), []);
        // A final line ending doesn't start another row; a blank line does.
        assert_eq!(rows(b"a\n", b','), [(1, Some(Lf))]);
        assert_eq!(rows(b"a\n\n", b','), [(1, Some(Lf)), (1, Some(Lf))]);
    }

    #[test]
    fn quotes_open_only_at_a_field_start() {
        // Inside quotes, delimiters and newlines are text.
        assert_eq!(
            rows(b"\"a,\nb\",c\nd\n", b','),
            [(2, Some(Lf)), (1, Some(Lf))]
        );
        // An escaped quote doesn't close the field.
        assert_eq!(rows(b"\"a\"\",b\",c\n", b','), [(2, Some(Lf))]);
        // A quote in the middle of a field is literal.
        assert_eq!(rows(b"a\"b,c\nd\n", b','), [(2, Some(Lf)), (1, Some(Lf))]);
        // Text after a closing quote is literal, quotes included.
        assert_eq!(rows(b"\"a\"b\"c,d\n", b','), [(2, Some(Lf))]);
        // A quote that never closes runs to the end: no line ending.
        assert_eq!(rows(b"a,\"b\nc\n", b','), [(2, None)]);
        // `"` then `"` at the very end: an empty quoted field, closed.
        assert_eq!(rows(b"\"\"", b','), [(1, None)]);
    }

    #[test]
    fn a_cut_sample_drops_its_unfinished_last_row() {
        let u = |t: &'static [u8]| Units::new(t, Encoding::Utf8);
        let endings = |t, cut| -> Vec<Option<LineEnding>> {
            whole_rows(u(t), b',', cut)
                .iter()
                .map(|r| r.ending)
                .collect()
        };
        assert_eq!(endings(b"a\nb", true), [Some(Lf)]);
        assert_eq!(endings(b"a\nb", false), [Some(Lf), None]);
        assert_eq!(endings(b"a\nb\r", true), [Some(Lf)]);
        assert_eq!(endings(b"a\nb\r", false), [Some(Lf), Some(Cr)]);
        assert_eq!(endings(b"a\nb\n", true), [Some(Lf), Some(Lf)]);
        assert_eq!(endings(b"a\n\"b\n", true), [Some(Lf)]);
        // A row longer than the window is kept: it is all there is.
        assert_eq!(endings(b"a\tb\tc", true), [None]);
        assert_eq!(endings(b"a\tb\r", true), [None]);
    }

    /// Feeding a scanner unit by unit gives the same rows as the iterator,
    /// wherever CR LF, `""` and closing quotes fall.
    #[test]
    fn the_scanner_is_the_same_unit_by_unit() {
        let text = b"a,\"b\"\"c\",d\r\n\"x\ny\"z\r\rq,\"open";
        let units = Units::new(text, Encoding::Utf8);
        let rows: Vec<(usize, usize, Option<LineEnding>, bool)> = Rows::new(units, b',')
            .map(|r| (r.span.len(), r.fields, r.ending, r.irregular))
            .collect();
        let mut scanner = Scanner::new(b',');
        let mut fed = Vec::new();
        for &b in text {
            fed.extend(scanner.feed(u16::from(b)));
        }
        fed.extend(scanner.finish());
        let fed: Vec<_> = fed
            .into_iter()
            .map(|e| (e.len, e.fields, e.ending, e.irregular))
            .collect();
        assert_eq!(rows, fed);
        assert_eq!(
            rows,
            [
                (10, 3, Some(Crlf), false),
                (6, 1, Some(Cr), true),
                (0, 1, Some(Cr), false),
                (7, 2, None, true),
            ]
        );
    }

    #[test]
    fn field_values_are_display_values() {
        let text = b"plain,\"quo\"\"ted\",\"a\"b,,\"open";
        let units = Units::new(text, Encoding::Utf8);
        let row = Rows::new(units, b',').next().unwrap();
        assert_eq!(
            field_values(units, &row, b','),
            ["plain", "quo\"ted", "\"a\"b", "", "open"]
        );
        let utf16: Vec<u8> = "x;\"y;z\""
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let units = Units::new(&utf16, Encoding::Utf16Le);
        let row = Rows::new(units, b';').next().unwrap();
        assert_eq!(field_values(units, &row, b';'), ["x", "y;z"]);
    }
}
