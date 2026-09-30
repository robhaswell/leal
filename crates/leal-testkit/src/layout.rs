//! The expected structure of a parsed file: rows, field spans and values.
//!
//! A [`Layout`] is what the model generator promises a parser will find, and
//! what later parser tests compare against. All coordinates follow the
//! conventions in `tests/corpus/README.md`:
//!
//! - **Rows** are *physical rows* (records), numbered from 0. A newline inside
//!   a quoted field does not start a new row. The BOM is not part of any row.
//!   A line ending at the very end of the file ends the last row; it does not
//!   start an empty one. An empty file (or one holding only a BOM) has no rows.
//! - **Offsets** are byte offsets into the file as stored on disk, from 0,
//!   *including* any BOM. Spans are half-open ranges, `start..end`.

use std::ops::Range;

use crate::dialect::{Delimiter, LineEnding};

/// The structure of one file.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Layout {
    /// Length of the BOM at the start of the file (0 or 3 for UTF-8 files).
    pub bom_len: usize,
    /// The file's rows, in order.
    pub rows: Vec<RowLayout>,
}

/// One physical row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowLayout {
    /// The row's bytes, *excluding* its line ending.
    pub span: Range<usize>,
    /// The line ending that ends the row, or `None` if the row runs to the
    /// end of the file.
    pub line_ending: Option<LineEnding>,
    /// The row's fields. Every row has at least one; a blank line is one
    /// empty, unquoted field.
    pub fields: Vec<FieldLayout>,
}

/// One field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldLayout {
    /// The field's raw bytes: quotes, escapes and any text after the closing
    /// quote included; the delimiter and line ending excluded.
    pub span: Range<usize>,
    /// True if the field's first byte is `"`.
    pub quoted: bool,
    /// The display value as bytes, before decoding:
    ///
    /// - unquoted: the raw bytes;
    /// - quoted: the text between the quotes, with `""` turned into `"`;
    /// - quoted with text after the closing quote (`"a"b`): the raw bytes,
    ///   exactly as written, so `"a"b` (ADR-0002 question 6, ADR-0003
    ///   decision 2);
    /// - unterminated: everything after the opening quote, with `""`
    ///   turned into `"`.
    pub value: Vec<u8>,
    /// Offset of the first byte after the closing quote, if the field has
    /// text between its closing quote and the next delimiter or line ending
    /// (`"a"b`).
    pub text_after_quote: Option<usize>,
    /// True if the field's opening quote is never closed. Such a field runs
    /// to the end of the file, so it is always the last field of the last row.
    pub unterminated: bool,
}

impl Layout {
    /// True if the last row ends with a line ending. False for a file with
    /// no rows, and for a file whose last row is an unterminated quoted field
    /// (the final newline, if any, is inside the field).
    #[must_use]
    pub fn trailing_newline(&self) -> bool {
        self.rows.last().is_some_and(|r| r.line_ending.is_some())
    }

    /// The most common line ending (ties go to the one seen first), and
    /// whether more than one kind is present. `(None, false)` if no row has a
    /// line ending.
    #[must_use]
    pub fn line_endings(&self) -> (Option<LineEnding>, bool) {
        let endings: Vec<LineEnding> = self.rows.iter().filter_map(|r| r.line_ending).collect();
        let dominant = mode(endings.iter().copied());
        let mixed = endings.iter().any(|e| Some(*e) != dominant);
        (dominant, mixed)
    }

    /// The number of fields in each row.
    #[must_use]
    pub fn field_counts(&self) -> Vec<usize> {
        self.rows.iter().map(|r| r.fields.len()).collect()
    }

    /// The most common field count among non-blank rows (ties go to the one
    /// seen first), or `None` if every row is blank.
    #[must_use]
    pub fn field_count_mode(&self) -> Option<usize> {
        mode(
            self.rows
                .iter()
                .filter(|r| !r.is_blank())
                .map(|r| r.fields.len()),
        )
    }

    /// The index of the row containing byte `offset`, counting a row's line
    /// ending as part of it. `None` for offsets in the BOM or past the end.
    #[must_use]
    pub fn row_of_offset(&self, offset: usize) -> Option<usize> {
        let idx = self.rows.partition_point(|r| r.span.start <= offset);
        let row = idx.checked_sub(1)?;
        let r = &self.rows[row];
        let end = r.span.end + r.line_ending.map_or(0, LineEnding::byte_len);
        (offset < end).then_some(row)
    }

    /// The `(row, field)` whose span contains byte `offset`. `None` for
    /// delimiters, line endings, the BOM and offsets past the end.
    #[must_use]
    pub fn field_of_offset(&self, offset: usize) -> Option<(usize, usize)> {
        let row = self.row_of_offset(offset)?;
        let field = self.rows[row]
            .fields
            .iter()
            .position(|f| f.span.contains(&offset))?;
        Some((row, field))
    }

    /// Checks the structural invariants every parse must satisfy: rows and
    /// fields exactly tile the file, with one delimiter byte between fields,
    /// each row's line ending present in the bytes, and a row without a line
    /// ending only at the end.
    ///
    /// # Errors
    ///
    /// Returns a description of the first broken invariant.
    pub fn check_tiles(&self, bytes: &[u8], delimiter: Delimiter) -> Result<(), String> {
        let mut pos = self.bom_len;
        for (ri, row) in self.rows.iter().enumerate() {
            if row.span.start != pos {
                return Err(format!(
                    "row {ri} starts at {}, expected {pos}",
                    row.span.start
                ));
            }
            if row.fields.is_empty() {
                return Err(format!("row {ri} has no fields"));
            }
            for (fi, field) in row.fields.iter().enumerate() {
                if fi > 0 {
                    if bytes.get(pos) != Some(&delimiter.byte()) {
                        return Err(format!(
                            "row {ri}: no delimiter before field {fi} at offset {pos}"
                        ));
                    }
                    pos += 1;
                }
                if field.span.start != pos || field.span.end < pos {
                    return Err(format!(
                        "row {ri} field {fi} has span {:?}, expected it to start at {pos}",
                        field.span
                    ));
                }
                pos = field.span.end;
            }
            if row.span.end != pos {
                return Err(format!(
                    "row {ri} ends at {}, fields end at {pos}",
                    row.span.end
                ));
            }
            match row.line_ending {
                Some(le) => {
                    if bytes.get(pos..pos + le.byte_len()) != Some(le.bytes()) {
                        return Err(format!("row {ri}: no {le:?} at offset {pos}"));
                    }
                    pos += le.byte_len();
                }
                None if ri + 1 != self.rows.len() => {
                    return Err(format!(
                        "row {ri} has no line ending but is not the last row"
                    ));
                }
                None => {}
            }
        }
        if pos != bytes.len() {
            return Err(format!(
                "rows end at offset {pos}, but the file is {} bytes",
                bytes.len()
            ));
        }
        Ok(())
    }
}

impl RowLayout {
    /// True if the row has no bytes at all before its line ending.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.span.is_empty()
    }
}

/// The most common item, with ties going to the item seen first.
pub(crate) fn mode<T: PartialEq + Copy>(items: impl Iterator<Item = T>) -> Option<T> {
    // (item, count) in first-seen order. The sets here are tiny (at most a
    // handful of line endings or field counts), so a Vec beats a map.
    let mut counts: Vec<(T, usize)> = Vec::new();
    for item in items {
        match counts.iter_mut().find(|(t, _)| *t == item) {
            Some((_, n)) => *n += 1,
            None => counts.push((item, 1)),
        }
    }
    let mut best: Option<(T, usize)> = None;
    for (item, n) in counts {
        if best.is_none_or(|(_, b)| n > b) {
            best = Some((item, n));
        }
    }
    best.map(|(item, _)| item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(span: Range<usize>, value: &[u8]) -> FieldLayout {
        FieldLayout {
            span,
            quoted: false,
            value: value.to_vec(),
            text_after_quote: None,
            unterminated: false,
        }
    }

    /// `a,b\n\nc` with no trailing newline.
    fn sample() -> Layout {
        Layout {
            bom_len: 0,
            rows: vec![
                RowLayout {
                    span: 0..3,
                    line_ending: Some(LineEnding::Lf),
                    fields: vec![field(0..1, b"a"), field(2..3, b"b")],
                },
                RowLayout {
                    span: 4..4,
                    line_ending: Some(LineEnding::Crlf),
                    fields: vec![field(4..4, b"")],
                },
                RowLayout {
                    span: 6..7,
                    line_ending: None,
                    fields: vec![field(6..7, b"c")],
                },
            ],
        }
    }

    #[test]
    fn mode_prefers_first_seen_on_ties() {
        assert_eq!(mode([2, 3, 3, 2].into_iter()), Some(2));
        assert_eq!(mode([2, 3, 3].into_iter()), Some(3));
        assert_eq!(mode(std::iter::empty::<u8>()), None);
    }

    #[test]
    fn derived_properties() {
        let l = sample();
        assert!(!l.trailing_newline());
        assert_eq!(l.line_endings(), (Some(LineEnding::Lf), true));
        assert_eq!(l.field_counts(), vec![2, 1, 1]);
        // The blank row is ignored; 2 and 1 tie, and 2 was seen first.
        assert_eq!(l.field_count_mode(), Some(2));
        assert!(l.rows[1].is_blank());
    }

    #[test]
    fn row_of_offset_includes_line_endings() {
        let l = sample();
        assert_eq!(l.row_of_offset(0), Some(0));
        assert_eq!(l.row_of_offset(3), Some(0)); // the LF
        assert_eq!(l.row_of_offset(4), Some(1)); // the CR of the blank row
        assert_eq!(l.row_of_offset(5), Some(1)); // its LF
        assert_eq!(l.row_of_offset(6), Some(2));
        assert_eq!(l.row_of_offset(7), None);
    }

    #[test]
    fn field_of_offset_skips_delimiters_and_line_endings() {
        let l = sample();
        assert_eq!(l.field_of_offset(0), Some((0, 0)));
        assert_eq!(l.field_of_offset(1), None); // the comma
        assert_eq!(l.field_of_offset(2), Some((0, 1)));
        assert_eq!(l.field_of_offset(3), None); // the LF
        assert_eq!(l.field_of_offset(4), None); // a blank row's field has no bytes
        assert_eq!(l.field_of_offset(6), Some((2, 0)));
    }

    #[test]
    fn check_tiles_accepts_a_valid_layout() {
        assert_eq!(
            sample().check_tiles(b"a,b\n\r\nc", Delimiter::Comma),
            Ok(())
        );
    }

    #[test]
    fn check_tiles_rejects_gaps_and_wrong_bytes() {
        let bytes = b"a,b\n\r\nc";
        let mut l = sample();
        l.rows[2].span = 7..7;
        assert!(l.check_tiles(bytes, Delimiter::Comma).is_err());

        let l = sample();
        let err = l.check_tiles(bytes, Delimiter::Semicolon).unwrap_err();
        assert!(err.contains("no delimiter"), "{err}");

        let err = l.check_tiles(b"a,b\n\r\ncd", Delimiter::Comma).unwrap_err();
        assert!(err.contains("file is 8 bytes"), "{err}");

        let mut l = sample();
        l.rows[0].line_ending = None;
        assert!(l.check_tiles(bytes, Delimiter::Comma).is_err());
    }
}
