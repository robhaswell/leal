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

/// The rows of `units`, read with `delimiter`, starting outside quotes.
pub(crate) struct Rows<'a> {
    units: Units<'a>,
    delimiter: u16,
    pos: usize,
}

impl<'a> Rows<'a> {
    pub(crate) fn new(units: Units<'a>, delimiter: u8) -> Self {
        Rows {
            units,
            delimiter: u16::from(delimiter),
            pos: 0,
        }
    }
}

impl Iterator for Rows<'_> {
    type Item = Row;

    fn next(&mut self) -> Option<Row> {
        let len = self.units.len();
        if self.pos >= len {
            return None;
        }
        let start = self.pos;
        let mut fields = 1;
        let mut field_start = true;
        let mut in_quotes = false;
        let mut just_closed = false;
        let mut irregular = false;
        let mut i = start;
        while i < len {
            let u = self.units.get(i);
            if in_quotes {
                if u == QUOTE_UNIT {
                    if self.units.get(i + 1) == QUOTE_UNIT && i + 1 < len {
                        i += 2; // an escaped quote
                        continue;
                    }
                    in_quotes = false; // what follows is literal text
                    just_closed = true;
                }
                i += 1;
                continue;
            }
            let after_quote = std::mem::take(&mut just_closed);
            if u == QUOTE_UNIT && field_start {
                in_quotes = true;
                field_start = false;
                i += 1;
            } else if u == self.delimiter {
                fields += 1;
                field_start = true;
                i += 1;
            } else if u == CR || u == LF {
                let crlf = u == CR && i + 1 < len && self.units.get(i + 1) == LF;
                let (ending, width) = match (u, crlf) {
                    (LF, _) => (LineEnding::Lf, 1),
                    (_, true) => (LineEnding::Crlf, 2),
                    _ => (LineEnding::Cr, 1),
                };
                self.pos = i + width;
                return Some(Row {
                    span: start..i,
                    fields,
                    ending: Some(ending),
                    irregular,
                });
            } else {
                // Text after a closing quote, or a stray quote mid-field.
                irregular |= after_quote || u == QUOTE_UNIT;
                field_start = false;
                i += 1;
            }
        }
        self.pos = len;
        Some(Row {
            span: start..len,
            fields,
            ending: None,
            irregular: irregular || in_quotes,
        })
    }
}

/// The rows of a sample that are known to be whole. If the sample was cut
/// short (`cut`), the last row may be missing its end: a row with no line
/// ending, or one ending in a CR at the very end (which could be the first
/// half of a CR LF), is dropped.
pub(crate) fn whole_rows(units: Units<'_>, delimiter: u8, cut: bool) -> Vec<Row> {
    let len = units.len();
    let mut rows: Vec<Row> = Rows::new(units, delimiter).collect();
    if cut && let Some(last) = rows.last() {
        let ends_in_cr = last.ending == Some(LineEnding::Cr) && last.span.end + 1 == len;
        if last.ending.is_none() || ends_in_cr {
            rows.pop();
        }
    }
    rows
}

/// Where a sample taken from the middle of the text starts: just after
/// its first line ending, so that its first (probably partial) row is
/// skipped. `None` if it has no line ending.
///
/// Whether that line ending was inside a quoted field can't be known
/// without reading from the start of the file. If it was, the sample's
/// first rows are misread, which is acceptable for a suggestion: quotes
/// only open at a field's start, so a wrong guess rarely spreads far.
pub(crate) fn after_first_line_ending(units: Units<'_>) -> Option<usize> {
    let len = units.len();
    let i = (0..len).find(|&i| matches!(units.get(i), CR | LF))?;
    let crlf = units.get(i) == CR && i + 1 < len && units.get(i + 1) == LF;
    Some(if crlf { i + 2 } else { i + 1 })
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
        assert_eq!(whole_rows(u(b"a\nb"), b',', true).len(), 1);
        assert_eq!(whole_rows(u(b"a\nb"), b',', false).len(), 2);
        assert_eq!(whole_rows(u(b"a\nb\r"), b',', true).len(), 1);
        assert_eq!(whole_rows(u(b"a\nb\r"), b',', false).len(), 2);
        assert_eq!(whole_rows(u(b"a\nb\n"), b',', true).len(), 2);
        assert_eq!(whole_rows(u(b"a\n\"b\n"), b',', true).len(), 1);
    }

    #[test]
    fn a_middle_sample_starts_after_its_first_line_ending() {
        let u = |t: &'static [u8]| Units::new(t, Encoding::Utf8);
        assert_eq!(after_first_line_ending(u(b"tail\r\nnext")), Some(6));
        assert_eq!(after_first_line_ending(u(b"\nnext")), Some(1));
        assert_eq!(after_first_line_ending(u(b"tail\rnext")), Some(5));
        assert_eq!(after_first_line_ending(u(b"no ending")), None);
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
