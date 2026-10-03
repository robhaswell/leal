//! The expected result of editing and saving a file (DESIGN §3.6, §3.7 and
//! fidelity rules F2–F6).
//!
//! This is an independent statement of the save rules for tasks 2.1–2.4, so
//! that the serializer is never tested against its own quoting code.
//!
//! - [`expected_field_bytes`] is §3.7 rule 2 for one edited field.
//! - [`Document`] replays [`Edit`]s against a parsed file, then
//!   [`Document::save`] gives the exact expected output, as a list of
//!   [`Change`]s to the original (for `assert_only_changed`) and as bytes.
//!
//! # The rules, as implemented
//!
//! 1. An untouched row is copied byte for byte, line ending included.
//! 2. A row with any change is rebuilt: untouched fields keep their raw
//!    bytes, edited fields are written by [`expected_field_bytes`], and the
//!    delimiters and line ending are the row's own.
//! 3. A new row uses the most common line ending (LF if the file has none)
//!    and the file's quoting style. A **new field** (an inserted row's, or
//!    an inserted column's) is quoted if it needs it, if the file quotes
//!    every field, or if its column quotes every field: the column has at
//!    least one non-empty field and every non-empty field in it is quoted
//!    ([`column_quoting`](Document::column_quoting)). The column is the
//!    logical column as the document is now; its fields are the original
//!    fields at that position (edited ones judged by their original bytes)
//!    in rows that aren't original blank lines; new cells don't count. An
//!    empty field is one with no bytes in the file (`""` is non-empty).
//!    Hatched cells (rule 12) are new values in old rows: quoted only if
//!    needed or the file quotes every field. (ADR-0004 decision 2,
//!    ADR-0005 decision 3, ADR-0014 decision 4.)
//! 4. The trailing newline is kept as it was. If the file had none, the last
//!    row of the output has none, and a row that used to be last but no
//!    longer is gets the most common line ending.
//! 5. Setting a cell to its original display value removes the edit, so its
//!    original bytes come back (§3.6).
//! 6. Column insert and delete apply to every row that has that position:
//!    an insert at `c` needs at least `c` fields, a delete at `c` needs more
//!    than `c`. Shorter (ragged) rows are left alone, and so are blank
//!    lines: an original one (ADR-0004 decision 5), and a row that edits
//!    and column deletes have left with no cells (ADR-0014 decision 6).
//! 7. If an edited value can't be encoded, saving fails naming the cells
//!    (§3.7, F5). UTF-16 files are read-only in v1, so saving them fails.
//!    [`Document::save_as_utf8`] (ADR-0008 decision 7) writes the same
//!    rows in UTF-8 instead, from any encoding: every unedited field's text
//!    converted, the BOM a UTF-8 one if there was one; it fails naming the
//!    unedited cells whose bytes aren't text in the file's encoding.
//! 8. A row whose bytes would be empty is written as `""`, unless it was an
//!    original blank row, which stays blank (ADR-0004 decision 6). A blank
//!    row that ends up last in a file with no trailing newline would vanish,
//!    so it is written as `""` too.
//! 9. If the output's first field would start with BOM-like bytes
//!    (`EF BB BF`, which is U+FEFF, or `FF FE` or `FE FF`) in a file without
//!    a BOM, that field is written quoted (ADR-0004 decisions 7 and 10).
//! 10. An original, unedited unterminated field must stay the last thing in
//!     the file, or new bytes would land inside its quote (ADR-0004
//!     decision 8). Any edit that breaks this is refused with
//!     [`SaveError::AfterUnterminatedQuote`]: a row inserted after its row,
//!     a column or a hatched cell (rule 12) after it, or setting it back to
//!     its original value after something was added behind it. Editing it
//!     closes the quote.
//! 11. A lone CR directly followed by a blank LF row would read back as one
//!     CRLF, so the blank row's line ending becomes CR (ADR-0004
//!     decision 10). Only a blank row can start with LF.
//! 12. A **hatched cell**, past the end of its row (a short row's missing
//!     field, or any field of a blank line but its first), can be edited
//!     (ADR-0005 decision 2). The row then gains the delimiters needed to
//!     reach that column, and the value, at its end before its line ending:
//!     the cells in between have no bytes at all. A blank line edited in
//!     column *c* becomes a row of *c* + 1 fields. A hatched cell's value
//!     is empty, so setting one to `""` is no edit, and setting it back to
//!     `""` removes the edit and the row's padding with it (F3). Padding
//!     is only ever before an edited hatched cell, so a column delete that
//!     takes a row's last one takes the padding left at the row's end too
//!     (ADR-0014 decision 5): the row's own bytes come back, as if the
//!     hatched cell had been set back to `""`. After a save the padding is
//!     a field of the file and stays. Past an unterminated quote, rule 10
//!     refuses it.
//!
//! Rules 8, 9 and 11 are the "smallest extra change next to the edit" that
//! keeps ADR-0004 decision 10: reopening the saved file gives the same
//! rows, BOM and line endings. [`SavedFile::line_endings`] says which rows
//! a reopen must find. The encoding is kept by the encoding hint
//! ([`SavedFile::encoding_hint`], ADR-0004 decision 11).
//!
//! How these map to ADR-0004: rule 3 is decisions 1, 2 and 3; rule 4 is
//! decision 4; rule 5 is decision 9; rule 6 is decision 5; rules 8, 9 and 10
//! are decisions 6, 7 and 8; rules 9 and 11 are decision 10. Rule 12 is
//! ADR-0005 decision 2.

use std::fmt;

use crate::dialect::{
    Delimiter, Encoding, LineEnding, UTF8_BOM, UTF16BE_BOM, UTF16LE_BOM, decode_strict,
    decode_value, encode_value, expected_encoding,
};

/// Why a cell can't be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bad {
    /// Its new value holds a character the encoding can't represent.
    Unencodable,
    /// Its original bytes aren't text in the file's encoding (Save As
    /// UTF-8).
    Unconvertible,
}
use crate::fidelity::{Change, apply_changes};

/// How new values are quoted when written (rule 3): every one if the file
/// quotes every field; otherwise a new field also if its column quotes
/// every field.
#[derive(Clone, Debug, Default)]
struct Quoting {
    all: bool,
    columns: Vec<bool>,
}

impl Quoting {
    /// Whether a new field in logical column `column` is quoted (besides
    /// needing it).
    fn new_field(&self, column: usize) -> bool {
        self.all || self.columns.get(column).copied().unwrap_or(false)
    }
}

use crate::layout::{FieldLayout, Layout, RowLayout};

/// The furthest a hatched-cell edit may reach (rule 12): column
/// `COLUMN_LIMIT - 1`, unless the row is already longer. The same limit as
/// leal-core's `edit::COLUMN_LIMIT`, which guards against padding a row to
/// millions of cells by mistake.
pub const COLUMN_LIMIT: usize = 1 << 20;

/// Whether a value must be quoted wherever it is written: it contains the
/// delimiter, `"`, CR or LF (§3.7). Checked on the encoded bytes; the
/// structural bytes are ASCII in every editable encoding.
#[must_use]
pub fn needs_quotes(value: &[u8], delimiter: Delimiter) -> bool {
    value
        .iter()
        .any(|&b| b == delimiter.byte() || b == b'"' || b == b'\r' || b == b'\n')
}

/// The bytes §3.7 rule 2 writes for an edited field: `value` encoded in the
/// file's encoding, quoted if it needs quotes, or the original field was
/// quoted, or the file quotes every field. Embedded quotes are doubled.
///
/// ```
/// use leal_testkit::dialect::{Delimiter, Encoding};
/// use leal_testkit::save::expected_field_bytes;
///
/// let bytes = |v, quoted, all| {
///     expected_field_bytes(v, Encoding::Utf8, Delimiter::Comma, quoted, all).unwrap()
/// };
/// assert_eq!(bytes("plain", false, false), b"plain");
/// assert_eq!(bytes("a,b", false, false), b"\"a,b\"");
/// assert_eq!(bytes("say \"hi\"", false, false), b"\"say \"\"hi\"\"\"");
/// assert_eq!(bytes("plain", true, false), b"\"plain\"");
/// assert_eq!(bytes("", false, true), b"\"\"");
/// ```
///
/// # Errors
///
/// Returns the first character the encoding can't represent (and any
/// character for UTF-16, which is read-only in v1).
pub fn expected_field_bytes(
    value: &str,
    encoding: Encoding,
    delimiter: Delimiter,
    original_quoted: bool,
    file_quotes_every_field: bool,
) -> Result<Vec<u8>, char> {
    let encoded = encode_value(value, encoding)?;
    if !(needs_quotes(&encoded, delimiter) || original_quoted || file_quotes_every_field) {
        return Ok(encoded);
    }
    Ok(quote(&encoded))
}

/// `"` + `value` with each `"` doubled + `"`.
fn quote(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(b'"');
    for &b in value {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
    out
}

/// One editing command, in logical coordinates (0-based, as the document is
/// *after* every earlier edit).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Set a cell's value.
    SetCell {
        /// Row.
        row: usize,
        /// Column: any, since a column past the row's end is a hatched
        /// cell (rule 12).
        column: usize,
        /// The new display value.
        value: String,
    },
    /// Insert a new row before row `at` (`at` = row count appends).
    InsertRow {
        /// Position.
        at: usize,
        /// The new row's values; at least one.
        values: Vec<String>,
    },
    /// Delete a row.
    DeleteRow {
        /// Row.
        row: usize,
    },
    /// Insert a column before column `at`, with `value` in every row that
    /// has at least `at` fields.
    InsertColumn {
        /// Position.
        at: usize,
        /// The value for every affected row.
        value: String,
    },
    /// Delete column `column` from every row that has it.
    DeleteColumn {
        /// Column.
        column: usize,
    },
}

/// Why an edit or a save failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    /// An edit's coordinates don't exist in the document.
    InvalidEdit(Edit),
    /// These cells (final logical row, column) hold characters the file's
    /// encoding can't represent (F5).
    Unencodable(Vec<(usize, usize)>),
    /// Save As UTF-8 (ADR-0008 decision 7): these cells (final logical
    /// row, column) are unedited fields whose bytes aren't text in the
    /// file's encoding (an unpaired surrogate or a final odd byte in
    /// UTF-16, a byte a single-byte encoding leaves unassigned), so they
    /// can't be converted (F5).
    Unconvertible(Vec<(usize, usize)>),
    /// UTF-16 files are read-only in v1.
    ReadOnly,
    /// The edit would put bytes inside an unterminated quote (ADR-0004
    /// decision 8).
    AfterUnterminatedQuote(Edit),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::InvalidEdit(e) => write!(f, "edit {e:?} is out of range"),
            SaveError::Unencodable(cells) => write!(f, "cells {cells:?} can't be encoded"),
            SaveError::Unconvertible(cells) => write!(f, "cells {cells:?} can't be converted"),
            SaveError::ReadOnly => f.write_str("UTF-16 files are read-only"),
            SaveError::AfterUnterminatedQuote(e) => {
                write!(f, "edit {e:?} would land inside an unterminated quote")
            }
        }
    }
}

impl std::error::Error for SaveError {}

/// The expected output of a save.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFile {
    /// The splices that turn the original into the output: per field for
    /// cell edits within an otherwise unchanged row, per row otherwise.
    pub changes: Vec<Change>,
    /// The complete expected output.
    pub bytes: Vec<u8>,
    /// Each output row's line ending, as the output is written. Reopening
    /// the output must find exactly these rows (ADR-0004 decision 10).
    pub line_endings: Vec<Option<LineEnding>>,
    /// The encoding hint the save must write (ADR-0004 decision 11): the
    /// document's encoding, when a reopen of `bytes` would otherwise guess a
    /// different one, or when the file already had a hint (which is then
    /// updated). `None` means write no hint. This models the macOS
    /// `com.apple.TextEncoding` extended attribute, which Leal marks as its
    /// own when a reopen would otherwise ignore it (ADR-0013 decision 2);
    /// reopen with [`crate::dialect::reopen_encoding`] and
    /// [`HintWriter::Leal`](crate::dialect::HintWriter::Leal).
    pub encoding_hint: Option<Encoding>,
    /// The extra changes the save had to make to keep the file's structure
    /// (ADR-0004 decisions 6, 7 and 10), in the order they were made. Tests
    /// can compare these with the serializer's, and coverage tests use them
    /// to check that each rule is exercised.
    pub fixes: Vec<Fix>,
    /// The output's rows and fields, as a parse of `bytes` finds them, so
    /// that the saved file can be edited in its turn ([`Document::new`]
    /// over `bytes`): undo after a save works on the saved file (ADR-0012
    /// decision 4, ADR-0014 decision 3). `None` for Save As UTF-8.
    pub layout: Option<Layout>,
}

/// An extra change a save makes so that the file reopens with the same
/// structure (ADR-0004 decisions 6, 7 and 10). Rows are output rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// A row that would have had no bytes is written as `""` (decision 6).
    EmptyRowQuoted {
        /// The output row.
        row: usize,
    },
    /// A blank row after a lone CR gets a CR line ending instead of LF, so
    /// the two don't read as one CRLF (decision 10).
    CrSplit {
        /// The output row whose line ending changed.
        row: usize,
    },
    /// The first field was quoted because it would start with BOM-like
    /// bytes (decisions 7 and 10).
    BomLikeQuoted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Cell {
    /// The original field with this index in the source row.
    Original(usize),
    /// A new value, replacing original field `field` if there was one.
    Edited { field: Option<usize>, value: String },
    /// A cell past the end of the row as it was (a hatched cell, rule 12):
    /// `None`, padding before an edited one, is written with no bytes;
    /// `Some` is its new value.
    Appended(Option<String>),
}

/// Where a cell of the document comes from now, for tests that compare a
/// reader with the oracle ([`Document::cell_source`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellSource {
    /// Original field `field` of the source row, unedited.
    Original {
        /// The field's index in the original row.
        field: usize,
    },
    /// An edited value.
    Edited,
    /// A cell past the end of the original row with no value of its own:
    /// padding before an edited hatched cell (rule 12). It reads as empty.
    Padding,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DocRow {
    /// The original row, or `None` for an inserted row.
    source: Option<usize>,
    cells: Vec<Cell>,
}

impl DocRow {
    /// An original blank line whose one (empty) cell hasn't been edited.
    fn is_blank_line(&self, layout: &Layout) -> bool {
        self.source.is_some_and(|s| layout.rows[s].is_blank()) && self.cells == [Cell::Original(0)]
    }
}

/// A file being edited: the original bytes plus the edits so far.
#[derive(Clone, Debug)]
pub struct Document<'a> {
    bytes: &'a [u8],
    layout: &'a Layout,
    delimiter: Delimiter,
    encoding: Encoding,
    /// The encoding hint the file had when it was opened, if any.
    existing_hint: Option<Encoding>,
    rows: Vec<DocRow>,
}

impl<'a> Document<'a> {
    /// Records that the file had an encoding hint when opened, so saving
    /// updates it (ADR-0004 decision 11). `encoding` passed to
    /// [`Document::new`] is the encoding the open chose, hint included.
    #[must_use]
    pub fn with_existing_hint(mut self, hint: Option<Encoding>) -> Self {
        self.existing_hint = hint;
        self
    }

    /// A document over `bytes`, whose parse is `layout`, with no edits.
    #[must_use]
    pub fn new(
        bytes: &'a [u8],
        layout: &'a Layout,
        delimiter: Delimiter,
        encoding: Encoding,
    ) -> Self {
        let rows = layout
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| DocRow {
                source: Some(i),
                cells: (0..r.fields.len()).map(Cell::Original).collect(),
            })
            .collect();
        Document {
            bytes,
            layout,
            delimiter,
            encoding,
            existing_hint: None,
            rows,
        }
    }

    /// Number of rows now.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of fields in row `row` now (0 if there is no such row).
    #[must_use]
    pub fn row_len(&self, row: usize) -> usize {
        self.rows.get(row).map_or(0, |r| r.cells.len())
    }

    /// The most fields any row has now.
    #[must_use]
    pub fn max_row_len(&self) -> usize {
        self.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0)
    }

    /// The most common row length now (ties go to the first seen), or 1.
    #[must_use]
    pub fn typical_row_len(&self) -> usize {
        crate::layout::mode(self.rows.iter().map(|r| r.cells.len()))
            .unwrap_or(1)
            .max(1)
    }

    /// The display value of a cell now. `None` past the end of the row (a
    /// hatched cell, whose value is empty) or of the document.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<String> {
        let r = self.rows.get(row)?;
        Some(match r.cells.get(column)? {
            Cell::Original(f) => self.field_display(r.source?, *f),
            Cell::Edited { value, .. } => value.clone(),
            Cell::Appended(value) => value.clone().unwrap_or_default(),
        })
    }

    /// Where cell (`row`, `column`) comes from now, or `None` past the end
    /// of the row or the document.
    #[must_use]
    pub fn cell_source(&self, row: usize, column: usize) -> Option<CellSource> {
        Some(match self.rows.get(row)?.cells.get(column)? {
            Cell::Original(field) => CellSource::Original { field: *field },
            Cell::Edited { .. } | Cell::Appended(Some(_)) => CellSource::Edited,
            Cell::Appended(None) => CellSource::Padding,
        })
    }

    /// Whether document row `row`, a row of the file, has all its own
    /// fields in their places (edited or not), then only hatched cells: no
    /// column insert or delete moved or took anything in it. `None` for an
    /// inserted row or past the end.
    #[must_use]
    pub fn same_shape(&self, row: usize) -> Option<bool> {
        let r = self.rows.get(row)?;
        let fields = self.layout.rows[r.source?].fields.len();
        Some(
            r.cells.len() >= fields
                && r.cells.iter().enumerate().all(|(k, cell)| match cell {
                    Cell::Original(f) => *f == k,
                    Cell::Edited { field, .. } => *field == Some(k),
                    Cell::Appended(_) => k >= fields,
                }),
        )
    }

    /// The original row behind document row `row`, or `None` for an
    /// inserted row or past the end.
    #[must_use]
    pub fn source_row(&self, row: usize) -> Option<usize> {
        self.rows.get(row)?.source
    }

    /// The original display value behind a cell, if it came from the file:
    /// empty for a cell past the end of an original row (a hatched cell,
    /// rule 12), whether or not it has been given a value or the row is that
    /// long now.
    #[must_use]
    pub fn original_value(&self, row: usize, column: usize) -> Option<String> {
        let r = self.rows.get(row)?;
        let source = r.source?;
        let field = match r.cells.get(column) {
            Some(Cell::Original(f)) => Some(*f),
            Some(Cell::Edited { field, .. }) => *field,
            Some(Cell::Appended(_)) | None => return Some(String::new()),
        }?;
        Some(self.field_display(source, field))
    }

    /// The (row, column) now of the original unterminated field, if it is
    /// still in the document unedited. Once edited, it is written with a
    /// closing quote, so nothing after it is swallowed.
    #[must_use]
    pub fn unterminated(&self) -> Option<(usize, usize)> {
        self.rows.iter().enumerate().find_map(|(ri, r)| {
            let source = &self.layout.rows[r.source?];
            r.cells
                .iter()
                .position(|c| matches!(c, Cell::Original(f) if source.fields[*f].unterminated))
                .map(|ci| (ri, ci))
        })
    }

    fn field_display(&self, row: usize, field: usize) -> String {
        decode_value(&self.layout.rows[row].fields[field].value, self.encoding)
    }

    /// Applies one edit. On error the document is unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`SaveError::InvalidEdit`] if its coordinates don't exist (a
    /// cell past [`COLUMN_LIMIT`] and its row's end doesn't), and
    /// [`SaveError::AfterUnterminatedQuote`] if afterwards an original
    /// unterminated field would no longer be the last thing in the file, so
    /// bytes would land inside its quote (ADR-0004 decision 8). That covers
    /// inserting a row after its row, a column or a hatched cell after it
    /// (ADR-0005 decision 2), and setting it back to its original value
    /// once something has been added after it. Inserting a row *at* its row
    /// index (before it) is allowed.
    pub fn apply(&mut self, edit: &Edit) -> Result<(), SaveError> {
        let before = self.rows.clone();
        self.apply_unchecked(edit)?;
        let swallows_nothing = self
            .unterminated()
            .is_none_or(|(r, c)| r + 1 == self.rows.len() && c + 1 == self.row_len(r));
        if swallows_nothing {
            Ok(())
        } else {
            self.rows = before;
            Err(SaveError::AfterUnterminatedQuote(edit.clone()))
        }
    }

    /// Deletes column `column` from rows `rows` alone, by value: what
    /// undoing a column insert after a save does (ADR-0014 decision 3),
    /// on the rows the insert gave a cell. Padding left at a row's end
    /// goes with it (ADR-0014 decision 5).
    ///
    /// # Errors
    ///
    /// [`SaveError::InvalidEdit`] (naming a `DeleteColumn`) if a row isn't
    /// there or doesn't have the column; nothing is changed then.
    pub fn delete_column_from(&mut self, column: usize, rows: &[usize]) -> Result<(), SaveError> {
        let invalid = || SaveError::InvalidEdit(Edit::DeleteColumn { column });
        if rows
            .iter()
            .any(|&r| self.rows.get(r).is_none_or(|row| row.cells.len() <= column))
        {
            return Err(invalid());
        }
        for &r in rows {
            let cells = &mut self.rows[r].cells;
            cells.remove(column);
            while cells.last() == Some(&Cell::Appended(None)) {
                cells.pop();
            }
        }
        Ok(())
    }

    /// Puts a column back at `at` by value, in each row of `cells` with its
    /// value: what undoing a column delete after a save does (ADR-0014
    /// decision 3). Each value is a new field (rule 3); a row now shorter
    /// than `at` is padded to it first, as for a hatched cell (rule 12).
    ///
    /// # Errors
    ///
    /// [`SaveError::InvalidEdit`] (naming an `InsertColumn`) if a row isn't
    /// there; nothing is changed then.
    pub fn restore_column(
        &mut self,
        at: usize,
        cells: &[(usize, String)],
    ) -> Result<(), SaveError> {
        if cells.iter().any(|&(r, _)| r >= self.rows.len()) {
            return Err(SaveError::InvalidEdit(Edit::InsertColumn {
                at,
                value: String::new(),
            }));
        }
        for (r, value) in cells {
            let row = &mut self.rows[*r].cells;
            if row.len() < at {
                row.resize(at, Cell::Appended(None));
            }
            let cell = Cell::Edited {
                field: None,
                value: value.clone(),
            };
            row.insert(at, cell);
        }
        Ok(())
    }

    fn apply_unchecked(&mut self, edit: &Edit) -> Result<(), SaveError> {
        let invalid = || SaveError::InvalidEdit(edit.clone());
        match edit {
            Edit::SetCell { row, column, value } => {
                let r = self.rows.get_mut(*row).ok_or_else(invalid)?;
                if *column >= r.cells.len().max(COLUMN_LIMIT) {
                    return Err(invalid());
                }
                if *column >= r.cells.len() {
                    // Rule 12: a hatched cell. Its value is empty, so `""`
                    // is no edit.
                    if !value.is_empty() {
                        r.cells.resize(*column, Cell::Appended(None));
                        r.cells.push(Cell::Appended(Some(value.clone())));
                    }
                    return Ok(());
                }
                if let Cell::Appended(old) = &mut r.cells[*column] {
                    *old = (!value.is_empty()).then(|| value.clone());
                    // Padding left at the end is no longer needed.
                    while r.cells.last() == Some(&Cell::Appended(None)) {
                        r.cells.pop();
                    }
                    return Ok(());
                }
                let source = r.source;
                let original = self.original_value(*row, *column);
                let cell = self
                    .rows
                    .get_mut(*row)
                    .and_then(|r| r.cells.get_mut(*column))
                    .ok_or_else(invalid)?;
                let field = match cell {
                    Cell::Original(f) => Some(*f),
                    Cell::Edited { field, .. } => *field,
                    Cell::Appended(_) => None,
                };
                *cell = match (field, source) {
                    // §3.6: back to the original display value removes the edit.
                    (Some(f), Some(_)) if original.as_deref() == Some(value.as_str()) => {
                        Cell::Original(f)
                    }
                    _ => Cell::Edited {
                        field,
                        value: value.clone(),
                    },
                };
            }
            Edit::InsertRow { at, values } => {
                if *at > self.rows.len() || values.is_empty() {
                    return Err(invalid());
                }
                let cells = values
                    .iter()
                    .map(|v| Cell::Edited {
                        field: None,
                        value: v.clone(),
                    })
                    .collect();
                self.rows.insert(
                    *at,
                    DocRow {
                        source: None,
                        cells,
                    },
                );
            }
            Edit::DeleteRow { row } => {
                if *row >= self.rows.len() {
                    return Err(invalid());
                }
                self.rows.remove(*row);
            }
            Edit::InsertColumn { at, value } => {
                if *at > self.max_row_len() {
                    return Err(invalid());
                }
                let layout = self.layout;
                for r in &mut self.rows {
                    // ADR-0004 decision 5: a blank line is too short for
                    // every column, so it never gains one. Nor does a row
                    // left with no cells, which is a blank line too
                    // (ADR-0014 decision 6).
                    let blank = r.cells.is_empty() || r.is_blank_line(layout);
                    if r.cells.len() >= *at && !blank {
                        let cell = Cell::Edited {
                            field: None,
                            value: value.clone(),
                        };
                        r.cells.insert(*at, cell);
                    }
                }
            }
            Edit::DeleteColumn { column } => {
                if *column >= self.max_row_len() {
                    return Err(invalid());
                }
                let layout = self.layout;
                for r in &mut self.rows {
                    if r.cells.len() > *column && !r.is_blank_line(layout) {
                        r.cells.remove(*column);
                        // Rule 12: padding is only before an edited
                        // hatched cell, so it goes with the row's last one
                        // (ADR-0014 decision 5).
                        while r.cells.last() == Some(&Cell::Appended(None)) {
                            r.cells.pop();
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The expected output of saving now.
    ///
    /// # Errors
    ///
    /// [`SaveError::ReadOnly`] for UTF-16; [`SaveError::Unencodable`] naming
    /// every cell whose value the encoding can't represent.
    pub fn save(&self) -> Result<SavedFile, SaveError> {
        if matches!(self.encoding, Encoding::Utf16Le | Encoding::Utf16Be) {
            return Err(SaveError::ReadOnly);
        }
        self.write(false)
    }

    /// The expected output of **Save As UTF-8** now (ADR-0008 decision 7):
    /// the same rows, fields, quoting, delimiters and line endings, every
    /// unedited field's text converted from the file's encoding and every
    /// edited value written in UTF-8, by the same rules as [`save`]
    /// (fixes included). A UTF-8 BOM only if the file had a BOM (of any
    /// encoding). `encoding_hint` is always UTF-8: the attribute is set.
    /// From a UTF-8 file, the same bytes as [`save`] (invalid bytes are kept
    /// as they are, F4).
    ///
    /// The bytes change throughout, so `changes` is empty: there are no
    /// splices to compare.
    ///
    /// [`save`]: Document::save
    ///
    /// # Errors
    ///
    /// [`SaveError::Unconvertible`] naming every unedited cell whose bytes
    /// aren't text in the file's encoding: an unpaired surrogate or a final
    /// odd byte in UTF-16, or a byte a single-byte encoding leaves
    /// unassigned. Nothing is substituted (F5).
    pub fn save_as_utf8(&self) -> Result<SavedFile, SaveError> {
        let mut saved = self.write(self.encoding != Encoding::Utf8)?;
        saved.encoding_hint = Some(Encoding::Utf8);
        Ok(saved)
    }

    /// The output of a save in the file's own encoding, or (`transcode`)
    /// converted to UTF-8.
    fn write(&self, transcode: bool) -> Result<SavedFile, SaveError> {
        let quoting = self.quoting();
        let dominant = self.layout.line_endings().0.unwrap_or(LineEnding::Lf);
        let trailing_newline = self.layout.trailing_newline();

        // Each row's content bytes, and whether it is byte-for-byte original.
        let mut unencodable = Vec::new();
        let mut unconvertible = Vec::new();
        let mut contents: Vec<(Vec<u8>, bool)> = Vec::with_capacity(self.rows.len());
        for (ri, row) in self.rows.iter().enumerate() {
            let untouched = row.source.is_some_and(|s| {
                row.cells.len() == self.layout.rows[s].fields.len()
                    && row
                        .cells
                        .iter()
                        .enumerate()
                        .all(|(i, c)| *c == Cell::Original(i))
            });
            let mut content = Vec::new();
            if untouched && !transcode {
                let span = self.layout.rows[row.source.unwrap_or(0)].span.clone();
                content.extend_from_slice(&self.bytes[span]);
            } else {
                for (ci, cell) in row.cells.iter().enumerate() {
                    if ci > 0 {
                        content.push(self.delimiter.byte());
                    }
                    match self.cell_bytes(row, ci, cell, &quoting, transcode) {
                        Ok(b) => content.extend_from_slice(&b),
                        Err(Bad::Unencodable) => unencodable.push((ri, ci)),
                        Err(Bad::Unconvertible) => unconvertible.push((ri, ci)),
                    }
                }
            }
            contents.push((content, untouched));
        }
        if !unencodable.is_empty() {
            return Err(SaveError::Unencodable(unencodable));
        }
        if !unconvertible.is_empty() {
            return Err(SaveError::Unconvertible(unconvertible));
        }

        // Line endings, then rule 4 for the end of the file.
        let last = self.rows.len().saturating_sub(1);
        let mut endings: Vec<Option<LineEnding>> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let own = r
                    .source
                    .map_or(Some(dominant), |s| self.layout.rows[s].line_ending);
                if i == last && !trailing_newline {
                    None
                } else {
                    own.or(Some(dominant))
                }
            })
            .collect();

        let mut fixes = Vec::new();

        // ADR-0004 decision 6: a row with no bytes becomes `""`, unless it was
        // an original blank row (and still has a line ending, so it survives).
        for (i, (content, untouched)) in contents.iter_mut().enumerate() {
            if !content.is_empty() {
                continue;
            }
            let was_blank = self.rows[i]
                .source
                .is_some_and(|s| self.layout.rows[s].is_blank());
            if !was_blank || endings[i].is_none() {
                *content = b"\"\"".to_vec();
                *untouched = false;
                fixes.push(Fix::EmptyRowQuoted { row: i });
            }
        }

        // ADR-0004 decision 10: a lone CR directly followed by an LF would
        // read back as one CRLF. Only a blank row can start with LF (every
        // other row starts with a field byte, and an unquoted field never
        // contains LF), so that blank row's line ending becomes CR. Going in
        // order lets a run of blank LF rows after a CR all become CR.
        for i in 1..endings.len() {
            if endings[i - 1] == Some(LineEnding::Cr)
                && contents[i].0.is_empty()
                && endings[i] == Some(LineEnding::Lf)
            {
                endings[i] = Some(LineEnding::Cr);
                fixes.push(Fix::CrSplit { row: i });
            }
        }

        // ADR-0004 decisions 7 and 10: in a file without a BOM, a first
        // field that would start with BOM-like bytes is quoted.
        if self.layout.bom_len == 0
            && let (Some(row), Some((content, untouched))) =
                (self.rows.first(), contents.first_mut())
            && [UTF8_BOM, UTF16LE_BOM, UTF16BE_BOM]
                .iter()
                .any(|bom| content.starts_with(bom))
            && let Some(cell) = row.cells.first()
            && let Ok(first) = self.cell_bytes(row, 0, cell, &quoting, transcode)
        {
            let rest = content[first.len()..].to_vec();
            *content = [quote(&first), rest].concat();
            *untouched = false;
            fixes.push(Fix::BomLikeQuoted);
        }

        // Serialize each row. The output is written out directly (BOM, then
        // every row), independently of the splices, so that tests can check
        // `apply_changes(original, changes) == bytes` for real.
        let serialized: Vec<Vec<u8>> = contents
            .iter()
            .zip(&endings)
            .map(|((c, _), e)| [c.as_slice(), e.map_or(&b""[..], LineEnding::bytes)].concat())
            .collect();
        // Save As UTF-8 writes a UTF-8 BOM if the file had any BOM.
        let bom = match (transcode, self.layout.bom_len) {
            (_, 0) => &b""[..],
            (true, _) => UTF8_BOM,
            (false, len) => &self.bytes[..len],
        };
        let bytes: Vec<u8> = std::iter::once(bom)
            .chain(serialized.iter().map(Vec::as_slice))
            .flatten()
            .copied()
            .collect();
        let (changes, layout) = if transcode {
            (Vec::new(), None)
        } else {
            let layout = self.layout(bom.len(), &contents, &endings, &fixes);
            (self.changes(&serialized, &contents, &endings), Some(layout))
        };
        // ADR-0004 decision 11: record the encoding when a reopen would
        // otherwise guess differently, or when the file already had a hint.
        let guess_differs = expected_encoding(&bytes) != self.encoding;
        let encoding_hint =
            (guess_differs || self.existing_hint.is_some()).then_some(self.encoding);
        Ok(SavedFile {
            changes,
            bytes,
            line_endings: endings,
            encoding_hint,
            fixes,
            layout,
        })
    }

    /// Which logical columns quote every field (rule 3, ADR-0005 decision
    /// 3, ADR-0014 decision 4): those with at least one non-empty original
    /// field, every one of them quoted. A column's fields are the original
    /// fields at its position now, edited ones by their original bytes, in
    /// rows that aren't original blank lines; new cells don't count.
    #[must_use]
    pub fn column_quoting(&self) -> Vec<bool> {
        // Per column: (a non-empty field seen, every one seen quoted).
        let mut columns: Vec<(bool, bool)> = Vec::new();
        for row in &self.rows {
            let Some(source) = row.source else { continue };
            if row.is_blank_line(self.layout) {
                continue;
            }
            for (column, cell) in row.cells.iter().enumerate() {
                let field = match cell {
                    Cell::Original(f) | Cell::Edited { field: Some(f), .. } => *f,
                    Cell::Edited { field: None, .. } | Cell::Appended(_) => continue,
                };
                let field = &self.layout.rows[source].fields[field];
                if field.span.is_empty() {
                    continue;
                }
                if columns.len() <= column {
                    columns.resize(column + 1, (false, true));
                }
                let entry = &mut columns[column];
                entry.0 = true;
                entry.1 &= field.quoted;
            }
        }
        columns
            .into_iter()
            .map(|(seen, quoted)| seen && quoted)
            .collect()
    }

    /// How new values are quoted now (rule 3).
    fn quoting(&self) -> Quoting {
        Quoting {
            all: self.layout.quotes_every_field(),
            columns: self.column_quoting(),
        }
    }

    /// The layout of the output whose rows' contents are `contents` and
    /// line endings `endings`, after `fixes`, for a BOM of `bom_len`
    /// bytes: each cell's bytes as written, at its place.
    fn layout(
        &self,
        bom_len: usize,
        contents: &[(Vec<u8>, bool)],
        endings: &[Option<LineEnding>],
        fixes: &[Fix],
    ) -> Layout {
        let quoting = self.quoting();
        let mut rows = Vec::with_capacity(self.rows.len());
        let mut at = bom_len;
        for (i, row) in self.rows.iter().enumerate() {
            let content = &contents[i].0;
            let start = at;
            let quoted_empty = fixes.contains(&Fix::EmptyRowQuoted { row: i });
            let mut fields = Vec::new();
            if quoted_empty {
                fields.push(FieldLayout {
                    span: start..start + 2,
                    quoted: true,
                    value: Vec::new(),
                    text_after_quote: None,
                    unterminated: false,
                });
            } else if content.is_empty() {
                fields.push(FieldLayout {
                    span: start..start,
                    quoted: false,
                    value: Vec::new(),
                    text_after_quote: None,
                    unterminated: false,
                });
            } else {
                let mut field_start = start;
                for (k, cell) in row.cells.iter().enumerate() {
                    let mut bytes = self
                        .cell_bytes(row, k, cell, &quoting, false)
                        .unwrap_or_default();
                    let bom_quoted = i == 0 && k == 0 && fixes.contains(&Fix::BomLikeQuoted);
                    if bom_quoted {
                        bytes = quote(&bytes);
                    }
                    let span = field_start..field_start + bytes.len();
                    let field = match (cell, row.source) {
                        (Cell::Original(f), Some(s)) if !bom_quoted => {
                            let old = &self.layout.rows[s].fields[*f];
                            FieldLayout {
                                span: span.clone(),
                                quoted: old.quoted,
                                value: old.value.clone(),
                                text_after_quote: old
                                    .text_after_quote
                                    .map(|t| t - old.span.start + span.start),
                                unterminated: old.unterminated,
                            }
                        }
                        (Cell::Original(f), Some(s)) => FieldLayout {
                            span: span.clone(),
                            quoted: true,
                            value: self.layout.rows[s].fields[*f].value.clone(),
                            text_after_quote: None,
                            unterminated: false,
                        },
                        _ => {
                            let value = match cell {
                                Cell::Edited { value, .. } | Cell::Appended(Some(value)) => {
                                    encode_value(value, self.encoding).unwrap_or_default()
                                }
                                _ => Vec::new(),
                            };
                            FieldLayout {
                                span: span.clone(),
                                quoted: bytes.first() == Some(&b'"'),
                                value,
                                text_after_quote: None,
                                unterminated: false,
                            }
                        }
                    };
                    fields.push(field);
                    field_start = span.end + 1;
                }
            }
            let end = start + content.len();
            debug_assert_eq!(fields.last().map(|f| f.span.end), Some(end));
            rows.push(RowLayout {
                span: start..end,
                line_ending: endings[i],
                fields,
            });
            at = end + endings[i].map_or(0, LineEnding::byte_len);
        }
        Layout { bom_len, rows }
    }

    /// A cell's bytes as written: an original field's raw bytes (converted
    /// to UTF-8 if `transcode`), or a new value encoded in the file's
    /// encoding (UTF-8 if `transcode`). `column` is the cell's logical
    /// column, for a new field's quoting.
    fn cell_bytes(
        &self,
        row: &DocRow,
        column: usize,
        cell: &Cell,
        quoting: &Quoting,
        transcode: bool,
    ) -> Result<Vec<u8>, Bad> {
        let target = if transcode {
            Encoding::Utf8
        } else {
            self.encoding
        };
        match cell {
            Cell::Original(f) => {
                let s = row.source.ok_or(Bad::Unconvertible)?;
                let raw = &self.bytes[self.layout.rows[s].fields[*f].span.clone()];
                if transcode {
                    decode_strict(raw, self.encoding)
                        .map(String::into_bytes)
                        .ok_or(Bad::Unconvertible)
                } else {
                    Ok(raw.to_vec())
                }
            }
            Cell::Appended(None) => Ok(Vec::new()),
            // A hatched cell keeps 2.2's rule (ADR-0014 decision 4).
            Cell::Appended(Some(value)) => {
                expected_field_bytes(value, target, self.delimiter, false, quoting.all)
                    .map_err(|_| Bad::Unencodable)
            }
            Cell::Edited { field, value } => {
                let quoted = match (row.source, field) {
                    (Some(s), Some(f)) => self.layout.rows[s].fields[*f].quoted || quoting.all,
                    // A new field: an inserted row's or column's (rule 3).
                    _ => quoting.new_field(column),
                };
                expected_field_bytes(value, target, self.delimiter, quoted, false)
                    .map_err(|_| Bad::Unencodable)
            }
        }
    }

    /// Splices from the original to the output. Rows keep their order, so
    /// the original rows that survive appear in the output in order, with
    /// inserted rows between them.
    fn changes(
        &self,
        serialized: &[Vec<u8>],
        contents: &[(Vec<u8>, bool)],
        endings: &[Option<LineEnding>],
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut pending: Vec<u8> = Vec::new(); // inserted rows not yet placed
        let mut j = 0;
        for (i, orig) in self.layout.rows.iter().enumerate() {
            let end = orig.span.end + orig.line_ending.map_or(0, LineEnding::byte_len);
            let range = orig.span.start..end;
            while j < self.rows.len() && self.rows[j].source.is_none() {
                pending.extend_from_slice(&serialized[j]);
                j += 1;
            }
            if !pending.is_empty() {
                changes.push(Change::insert(range.start, std::mem::take(&mut pending)));
            }
            if j < self.rows.len() && self.rows[j].source == Some(i) {
                if serialized[j] != self.bytes[range.clone()] {
                    self.row_changes(
                        i,
                        j,
                        &serialized[j],
                        contents[j].1,
                        endings[j],
                        &mut changes,
                    );
                }
                j += 1;
            } else {
                changes.push(Change::delete(range));
            }
        }
        while j < self.rows.len() {
            pending.extend_from_slice(&serialized[j]);
            j += 1;
        }
        if !pending.is_empty() {
            changes.push(Change::insert(self.bytes.len(), pending));
        }
        changes
    }

    /// Changes for surviving original row `i` (document row `j`): one per
    /// edited field, plus one insert at the row's end for edited hatched
    /// cells (rule 12, F2), if only cells changed; otherwise the whole row.
    fn row_changes(
        &self,
        i: usize,
        j: usize,
        serialized: &[u8],
        untouched: bool,
        ending: Option<LineEnding>,
        changes: &mut Vec<Change>,
    ) {
        let orig = &self.layout.rows[i];
        let cells = &self.rows[j].cells;
        let fields = orig.fields.len();
        let same_shape = !untouched
            && ending == orig.line_ending
            && cells.len() >= fields
            && cells.iter().enumerate().all(|(k, c)| match c {
                Cell::Original(f) => *f == k,
                Cell::Edited { field, .. } => *field == Some(k),
                Cell::Appended(_) => k >= fields,
            });
        let end = orig.span.end + orig.line_ending.map_or(0, LineEnding::byte_len);
        if same_shape {
            let quoting = self.quoting();
            let mut per_field = Vec::new();
            for (k, cell) in cells.iter().enumerate().take(fields) {
                if let Cell::Edited { .. } = cell {
                    let bytes = self
                        .cell_bytes(&self.rows[j], k, cell, &quoting, false)
                        .unwrap_or_default();
                    let span = orig.fields[k].span.clone();
                    if bytes != self.bytes[span.clone()] {
                        per_field.push(Change::replace(span, bytes));
                    }
                }
            }
            // Hatched cells: the delimiter before each, and its bytes, at
            // the end of the row before its line ending.
            let mut appended = Vec::new();
            for (k, cell) in cells.iter().enumerate().skip(fields) {
                appended.push(self.delimiter.byte());
                appended.extend(
                    self.cell_bytes(&self.rows[j], k, cell, &quoting, false)
                        .unwrap_or_default(),
                );
            }
            if !appended.is_empty() {
                per_field.push(Change::insert(orig.span.end, appended));
            }
            // Use the per-field splices only if they reproduce the row
            // exactly. Rules 8 and 9 (`""` rows, a quoted BOM-like first
            // field) can change a row in ways no single edited cell explains.
            let local: Vec<Change> = per_field
                .iter()
                .map(|c| {
                    let r = c.range.start - orig.span.start..c.range.end - orig.span.start;
                    Change::replace(r, c.replacement.clone())
                })
                .collect();
            if apply_changes(&self.bytes[orig.span.start..end], &local) == serialized {
                changes.extend(per_field);
                return;
            }
        }
        changes.push(Change::replace(orig.span.start..end, serialized.to_vec()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{FieldLayout, RowLayout};

    /// A tiny unquoted-only parser for these tests (the full oracle lives
    /// in the integration tests).
    fn simple_layout(bytes: &[u8], quoted: &[(usize, usize)]) -> Layout {
        let mut rows = Vec::new();
        let mut start = 0;
        for (ri, line) in bytes.split_inclusive(|&b| b == b'\n').enumerate() {
            let has_lf = line.last() == Some(&b'\n');
            let content = &line[..line.len() - usize::from(has_lf)];
            let mut fields = Vec::new();
            let mut fs = start;
            for (fi, f) in content.split(|&b| b == b',').enumerate() {
                let q = quoted.contains(&(ri, fi));
                let value = if q {
                    f[1..f.len() - 1].to_vec()
                } else {
                    f.to_vec()
                };
                fields.push(FieldLayout {
                    span: fs..fs + f.len(),
                    quoted: q,
                    value,
                    text_after_quote: None,
                    unterminated: false,
                });
                fs += f.len() + 1;
            }
            rows.push(RowLayout {
                span: start..start + content.len(),
                line_ending: has_lf.then_some(LineEnding::Lf),
                fields,
            });
            start += line.len();
        }
        Layout { bom_len: 0, rows }
    }

    fn save(
        bytes: &[u8],
        quoted: &[(usize, usize)],
        edits: &[Edit],
    ) -> Result<SavedFile, SaveError> {
        let layout = simple_layout(bytes, quoted);
        assert_eq!(layout.check_tiles(bytes, Delimiter::Comma), Ok(()));
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        for e in edits {
            doc.apply(e)?;
        }
        let saved = doc.save()?;
        // `bytes` and `changes` are built separately; they must agree.
        assert_eq!(apply_changes(bytes, &saved.changes), saved.bytes);
        Ok(saved)
    }

    fn set(row: usize, column: usize, value: &str) -> Edit {
        Edit::SetCell {
            row,
            column,
            value: value.to_owned(),
        }
    }

    fn out(r: Result<SavedFile, SaveError>) -> String {
        String::from_utf8(r.unwrap().bytes).unwrap()
    }

    #[test]
    fn no_edits_is_identical() {
        let r = save(b"a,b\n1,2\n", &[], &[]).unwrap();
        assert!(r.changes.is_empty());
        assert_eq!(r.bytes, b"a,b\n1,2\n");
    }

    #[test]
    fn cell_edits_quote_only_when_needed() {
        assert_eq!(
            out(save(b"a,b\n1,2\n", &[], &[set(1, 0, "x")])),
            "a,b\nx,2\n"
        );
        assert_eq!(
            out(save(b"a,b\n1,2\n", &[], &[set(1, 0, "x,y")])),
            "a,b\n\"x,y\",2\n"
        );
        assert_eq!(
            out(save(b"a,b\n1,2\n", &[], &[set(1, 1, "\"")])),
            "a,b\n1,\"\"\"\"\n"
        );
        assert_eq!(
            out(save(b"a,b\n1,2\n", &[], &[set(1, 1, "l\nf")])),
            "a,b\n1,\"l\nf\"\n"
        );
        // The original field was quoted, so the new value is too.
        assert_eq!(
            out(save(b"a,\"b\"\n", &[(0, 1)], &[set(0, 1, "c")])),
            "a,\"c\"\n"
        );
    }

    #[test]
    fn a_cell_edit_is_one_field_change() {
        let r = save(b"a,b\n1,2\n", &[], &[set(1, 1, "22")]).unwrap();
        assert_eq!(r.changes, vec![Change::replace(6..7, "22")]);
    }

    #[test]
    fn a_file_that_quotes_every_field_quotes_new_values() {
        let q = [(0, 0), (0, 1)];
        assert_eq!(
            out(save(b"\"a\",\"b\"\n", &q, &[set(0, 0, "x")])),
            "\"x\",\"b\"\n"
        );
        let r = save(
            b"\"a\",\"b\"\n",
            &q,
            &[Edit::InsertRow {
                at: 1,
                values: vec!["1".into(), "".into()],
            }],
        );
        assert_eq!(out(r), "\"a\",\"b\"\n\"1\",\"\"\n");
    }

    /// ADR-0004 decision 1: blank lines have no field bytes to quote, so
    /// they don't stop a file from quoting every field.
    #[test]
    fn a_blank_line_does_not_stop_a_file_quoting_every_field() {
        let q = [(0, 0), (0, 1), (2, 0), (2, 1)];
        let bytes = b"\"a\",\"b\"\n\n\"c\",\"d\"\n";
        assert_eq!(
            out(save(bytes, &q, &[set(2, 0, "x")])),
            "\"a\",\"b\"\n\n\"x\",\"d\"\n"
        );
        let ins = Edit::InsertRow {
            at: 3,
            values: vec!["1".into(), "2".into()],
        };
        assert_eq!(
            out(save(bytes, &q, &[ins])),
            "\"a\",\"b\"\n\n\"c\",\"d\"\n\"1\",\"2\"\n"
        );
    }

    /// ADR-0004 decision 3: a file with no line ending at all gives new rows
    /// LF.
    #[test]
    fn new_rows_in_a_file_with_no_line_ending_use_lf() {
        let ins = |at| Edit::InsertRow {
            at,
            values: vec!["x".into()],
        };
        assert_eq!(out(save(b"a", &[], &[ins(1)])), "a\nx");
        assert_eq!(out(save(b"a", &[], &[ins(0)])), "x\na");
        assert_eq!(out(save(b"a,b", &[], &[ins(1), ins(2)])), "a,b\nx\nx");
    }

    /// ADR-0004 decision 5: a blank line never loses its cell to a column
    /// delete, so a later column insert can't turn it into data.
    #[test]
    fn a_blank_line_survives_a_column_delete_then_insert() {
        let edits = [
            Edit::DeleteColumn { column: 0 },
            Edit::InsertColumn {
                at: 0,
                value: "N".into(),
            },
        ];
        let r = save(b"a,b\n\nc,d\n", &[], &edits).unwrap();
        assert_eq!(String::from_utf8(r.bytes).unwrap(), "N,b\n\nN,d\n");
    }

    /// Changes are per row: a row inserted between two untouched rows is
    /// one insert, and touches neither neighbour.
    #[test]
    fn an_inserted_row_is_one_insert_between_its_neighbours() {
        let ins = Edit::InsertRow {
            at: 1,
            values: vec!["x".into(), "y".into()],
        };
        let r = save(b"a,b\nc,d\n", &[], &[ins]).unwrap();
        assert_eq!(r.changes, vec![Change::insert(4, "x,y\n")]);
    }

    #[test]
    fn typical_row_len_is_the_most_common_row_length() {
        let bytes = b"a,b\nc,d\ne\n";
        let layout = simple_layout(bytes, &[]);
        let doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        assert_eq!(doc.typical_row_len(), 2);
        let layout = simple_layout(b"", &[]);
        let doc = Document::new(b"", &layout, Delimiter::Comma, Encoding::Utf8);
        assert_eq!(doc.typical_row_len(), 1, "an empty file still gets one");
    }

    #[test]
    fn reverting_restores_the_original_bytes() {
        let edits = [set(0, 1, "x"), set(0, 1, "b")];
        let r = save(b"a,\"b\"\n", &[(0, 1)], &edits).unwrap();
        assert!(r.changes.is_empty());
        assert_eq!(r.bytes, b"a,\"b\"\n");
    }

    #[test]
    fn rows_insert_and_delete_with_the_trailing_newline_kept() {
        let insert_end = Edit::InsertRow {
            at: 2,
            values: vec!["c".into()],
        };
        // No trailing newline: the old last row gains one, the new one has none.
        assert_eq!(
            out(save(b"a\nb", &[], std::slice::from_ref(&insert_end))),
            "a\nb\nc"
        );
        assert_eq!(out(save(b"a\nb\n", &[], &[insert_end])), "a\nb\nc\n");
        // Deleting the last row of a file with no trailing newline drops the
        // new last row's line ending.
        assert_eq!(out(save(b"a\nb", &[], &[Edit::DeleteRow { row: 1 }])), "a");
        assert_eq!(
            out(save(b"a\nb\nc\n", &[], &[Edit::DeleteRow { row: 1 }])),
            "a\nc\n"
        );
        let r = save(b"a\nb\n", &[], &[Edit::DeleteRow { row: 0 }]).unwrap();
        assert_eq!(r.changes, vec![Change::delete(0..2)]);
    }

    #[test]
    fn blank_lines_never_gain_or_lose_a_column() {
        // ADR-0004 decision 5: a blank line is too short for every column.
        let ins = Edit::InsertColumn {
            at: 0,
            value: "N".into(),
        };
        assert_eq!(out(save(b"a,b\n\nc,d\n", &[], &[ins])), "N,a,b\n\nN,c,d\n");
        let end = Edit::InsertColumn {
            at: 1,
            value: "N".into(),
        };
        assert_eq!(out(save(b"a\n\nc\n", &[], &[end])), "a,N\n\nc,N\n");
        let del = Edit::DeleteColumn { column: 0 };
        let r = save(b"a,b\n\nc,d\n", &[], &[del]).unwrap();
        assert_eq!(String::from_utf8(r.bytes).unwrap(), "b\n\nd\n");
        assert!(r.fixes.is_empty(), "the blank line stays blank, untouched");
        // Once its cell is edited, it is an ordinary one-field row again.
        let edit_then_insert = [set(1, 0, "x"), end_column(1)];
        assert_eq!(
            out(save(b"a\n\nc\n", &[], &edit_then_insert)),
            "a,N\nx,N\nc,N\n"
        );
    }

    fn end_column(at: usize) -> Edit {
        Edit::InsertColumn {
            at,
            value: "N".into(),
        }
    }

    #[test]
    fn columns_apply_to_rows_that_have_them() {
        let bytes = b"a,b,c\n1,2\n";
        let del = Edit::DeleteColumn { column: 2 };
        assert_eq!(out(save(bytes, &[], &[del])), "a,b\n1,2\n");
        let del0 = Edit::DeleteColumn { column: 0 };
        assert_eq!(out(save(bytes, &[], &[del0])), "b,c\n2\n");
        let ins = Edit::InsertColumn {
            at: 3,
            value: "n".into(),
        };
        // Row 1 has only 2 fields, so it is left alone.
        assert_eq!(out(save(bytes, &[], &[ins])), "a,b,c,n\n1,2\n");
        let ins0 = Edit::InsertColumn {
            at: 0,
            value: "x,y".into(),
        };
        assert_eq!(
            out(save(bytes, &[], &[ins0])),
            "\"x,y\",a,b,c\n\"x,y\",1,2\n"
        );
    }

    /// ADR-0014 decision 6: a row that column deletes have left with no
    /// cells is a blank line, which a column insert skips. It is still
    /// written as `""` (rule 8).
    #[test]
    fn a_row_left_with_no_cells_gains_no_inserted_column() {
        let bytes = b"a,b\nc\n";
        let del0 = Edit::DeleteColumn { column: 0 };
        let ins0 = Edit::InsertColumn {
            at: 0,
            value: "z".into(),
        };
        assert_eq!(out(save(bytes, &[], &[del0, ins0])), "z,b\n\"\"\n");
    }

    #[test]
    fn unencodable_and_read_only() {
        let bytes = b"a\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1252);
        doc.apply(&set(0, 0, "€ ok")).unwrap();
        assert_eq!(doc.save().unwrap().bytes, b"\x80 ok\n");
        doc.apply(&set(0, 0, "😀")).unwrap();
        assert_eq!(doc.save(), Err(SaveError::Unencodable(vec![(0, 0)])));
        let doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf16Le);
        assert_eq!(doc.save(), Err(SaveError::ReadOnly));
    }

    /// Every single-byte encoding saves its own characters, and refuses
    /// others, naming the cells.
    #[test]
    fn single_byte_encodings_save_their_own_characters() {
        let bytes = b"a,b\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1251);
        doc.apply(&set(0, 1, "Жук")).unwrap();
        assert_eq!(doc.save().unwrap().bytes, b"a,\xC6\xF3\xEA\n");
        doc.apply(&set(0, 0, "é")).unwrap();
        assert_eq!(doc.save(), Err(SaveError::Unencodable(vec![(0, 0)])));
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Iso8859_1);
        doc.apply(&set(0, 0, "\u{80}é")).unwrap();
        assert_eq!(doc.save().unwrap().bytes, b"\x80\xE9,b\n");
    }

    /// ADR-0008 decision 7: Save As UTF-8 converts every field, writes
    /// edits in UTF-8, and keeps delimiters, quoting and line endings.
    #[test]
    fn save_as_utf8_converts_every_field() {
        let bytes = b"caf\xE9,\"\x80;x\"\r\nb\n";
        let layout = simple_layout(bytes, &[(0, 1)]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1252);
        doc.apply(&set(1, 0, "😀")).unwrap();
        assert_eq!(doc.save(), Err(SaveError::Unencodable(vec![(1, 0)])));
        let saved = doc.save_as_utf8().unwrap();
        assert_eq!(saved.bytes, "café,\"€;x\"\r\n😀\n".as_bytes());
        assert_eq!(saved.encoding_hint, Some(Encoding::Utf8));
        assert!(saved.changes.is_empty());
        // From UTF-8: the bytes Save writes, invalid ones kept.
        let bytes = b"a,\xFF\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        doc.apply(&set(0, 0, "é")).unwrap();
        let saved = doc.save_as_utf8().unwrap();
        assert_eq!(saved.bytes, doc.save().unwrap().bytes);
        assert_eq!(saved.bytes, b"\xC3\xA9,\xFF\n");
    }

    /// A byte the encoding leaves unassigned can't be converted: the cell
    /// is named, and editing it lets the file convert (F5).
    #[test]
    fn save_as_utf8_names_the_cells_it_cant_convert() {
        let bytes = b"a,\xAA\n\xAAb,c\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1253);
        assert_eq!(
            doc.save_as_utf8(),
            Err(SaveError::Unconvertible(vec![(0, 1), (1, 0)]))
        );
        doc.apply(&set(0, 1, "x")).unwrap();
        doc.apply(&set(1, 0, "Ω")).unwrap();
        assert_eq!(doc.save_as_utf8().unwrap().bytes, "a,x\nΩ,c\n".as_bytes());
    }

    /// A UTF-16 file: a UTF-8 BOM for its BOM, and an unpaired surrogate or
    /// a final odd byte named by its cell.
    #[test]
    fn save_as_utf8_from_utf16() {
        use crate::strategies::csv::{
            CsvModel, GeneratedCsv, LineEndings, ModelDialect, ModelField, ModelRow, QuotingStyle,
            Utf16Faults,
        };
        let model = CsvModel {
            dialect: ModelDialect {
                delimiter: Delimiter::Semicolon,
                line_endings: LineEndings::Uniform(LineEnding::Crlf),
                bom: false,
                quoting: QuotingStyle::Minimal,
            },
            rows: vec![ModelRow {
                fields: vec![
                    ModelField::Unquoted("ab".as_bytes().to_vec()),
                    ModelField::Quoted {
                        value: "😀;\n".as_bytes().to_vec(),
                        trailing: Vec::new(),
                    },
                ],
                line_ending: Some(LineEnding::Crlf),
            }],
        };
        let file = GeneratedCsv::from_model(model.clone()).into_utf16(true);
        let doc = file.document();
        let saved = doc.save_as_utf8().unwrap();
        assert_eq!(saved.bytes, "\u{FEFF}ab;\"😀;\n\"\r\n".as_bytes());
        // The first character, "a", as an unpaired surrogate.
        let faults = Utf16Faults {
            lone_surrogates: vec![0],
            odd_byte: None,
        };
        let file = GeneratedCsv::from_model(model.clone()).into_utf16_with(false, &faults);
        assert_eq!(&file.bytes[..4], b"\xFE\xFF\xD8\x3D");
        let mut doc = file.document();
        assert_eq!(doc.value(0, 0).as_deref(), Some("\u{FFFD}b"));
        assert_eq!(
            doc.save_as_utf8(),
            Err(SaveError::Unconvertible(vec![(0, 0)]))
        );
        doc.apply(&set(0, 0, "fixed")).unwrap();
        assert_eq!(
            doc.save_as_utf8().unwrap().bytes,
            "\u{FEFF}fixed;\"😀;\n\"\r\n".as_bytes()
        );
        // A final odd byte: a row of its own after the CRLF.
        let faults = Utf16Faults {
            lone_surrogates: Vec::new(),
            odd_byte: Some(b'A'),
        };
        let file = GeneratedCsv::from_model(model).into_utf16_with(true, &faults);
        let doc = file.document();
        assert_eq!(doc.row_count(), 2);
        assert_eq!(
            doc.save_as_utf8(),
            Err(SaveError::Unconvertible(vec![(1, 0)]))
        );
    }

    /// The fixes apply to the UTF-8 written: a first field starting with
    /// U+FEFF is quoted in a file that had no BOM.
    #[test]
    fn save_as_utf8_quotes_a_bom_like_first_field() {
        let bytes = b"a,b\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1252);
        doc.apply(&set(0, 0, "\u{FEFF}x")).unwrap();
        let saved = doc.save_as_utf8().unwrap();
        assert_eq!(saved.bytes, "\"\u{FEFF}x\",b\n".as_bytes());
        assert_eq!(saved.fixes, [Fix::BomLikeQuoted]);
    }

    /// ADR-0004 decision 6.
    #[test]
    fn rows_that_would_have_no_bytes_are_written_as_empty_quotes() {
        // Clearing the only field of a one-column file.
        assert_eq!(out(save(b"a\nb\n", &[], &[set(0, 0, "")])), "\"\"\nb\n");
        // At the end of a file with no trailing newline, it would vanish.
        assert_eq!(out(save(b"a\nb", &[], &[set(1, 0, "")])), "a\n\"\"");
        // Deleting the only column.
        let del = Edit::DeleteColumn { column: 0 };
        assert_eq!(
            out(save(b"a\nb\n", &[], std::slice::from_ref(&del))),
            "\"\"\n\"\"\n"
        );
        // A new row with one empty value.
        let ins = Edit::InsertRow {
            at: 1,
            values: vec![String::new()],
        };
        assert_eq!(out(save(b"a\nb\n", &[], &[ins])), "a\n\"\"\nb\n");
        // An original blank row stays blank, even when its row is rebuilt.
        assert_eq!(out(save(b"a\n\nb\n", &[], &[set(0, 0, "x")])), "x\n\nb\n");
        assert_eq!(out(save(b"a\n\nb\n", &[], &[del])), "\"\"\n\n\"\"\n");
        // ...unless it would become the last row of a file with no trailing
        // newline, where it would vanish.
        let drop_last = Edit::DeleteRow { row: 2 };
        assert_eq!(out(save(b"a\n\nb", &[], &[drop_last])), "a\n\"\"");
    }

    /// ADR-0004 decision 10: deleting the row between a CR-terminated row
    /// and blank LF rows would put CR and LF next to each other, which reads
    /// back as one CRLF. The blank rows' line endings become CR instead.
    #[test]
    fn a_lone_cr_never_joins_a_following_lf() {
        let bytes = b"a\rb\n\n\nc\n";
        let layout = Layout {
            bom_len: 0,
            rows: vec![
                row_layout(0..1, LineEnding::Cr, b"a"),
                row_layout(2..3, LineEnding::Lf, b"b"),
                row_layout(4..4, LineEnding::Lf, b""),
                row_layout(5..5, LineEnding::Lf, b""),
                row_layout(6..7, LineEnding::Lf, b"c"),
            ],
        };
        assert_eq!(layout.check_tiles(bytes, Delimiter::Comma), Ok(()));
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        doc.apply(&Edit::DeleteRow { row: 1 }).unwrap();
        let saved = doc.save().unwrap();
        assert_eq!(saved.bytes, b"a\r\r\rc\n");
        let cr = Some(LineEnding::Cr);
        assert_eq!(saved.line_endings, vec![cr, cr, cr, Some(LineEnding::Lf)]);
        // A non-blank row after the CR can't join it: it starts with a field
        // byte, never LF.
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        doc.apply(&Edit::DeleteRow { row: 3 }).unwrap();
        doc.apply(&Edit::DeleteRow { row: 2 }).unwrap();
        doc.apply(&Edit::DeleteRow { row: 1 }).unwrap();
        assert_eq!(doc.save().unwrap().bytes, b"a\rc\n");
    }

    /// ADR-0004 decision 10: `FF FE` or `FE FF` at the start of a
    /// single-byte file would be read as a UTF-16 BOM.
    #[test]
    fn utf16_bom_bytes_moved_to_the_start_are_quoted() {
        for bom in [UTF16LE_BOM, UTF16BE_BOM] {
            let bytes = [b"x\n", bom, b"y\n"].concat();
            let layout = Layout {
                bom_len: 0,
                rows: vec![
                    row_layout(0..1, LineEnding::Lf, b"x"),
                    row_layout(2..5, LineEnding::Lf, &bytes[2..5]),
                ],
            };
            let mut doc = Document::new(&bytes, &layout, Delimiter::Comma, Encoding::Windows1252);
            doc.apply(&Edit::DeleteRow { row: 0 }).unwrap();
            let saved = doc.save().unwrap();
            assert_eq!(saved.bytes, [b"\"", bom, b"y\"\n"].concat(), "{bom:x?}");
            assert_eq!(saved.fixes, vec![Fix::BomLikeQuoted], "{bom:x?}");
        }
    }

    fn row_layout(span: std::ops::Range<usize>, le: LineEnding, value: &[u8]) -> RowLayout {
        RowLayout {
            span: span.clone(),
            line_ending: Some(le),
            fields: vec![FieldLayout {
                span,
                quoted: false,
                value: value.to_vec(),
                text_after_quote: None,
                unterminated: false,
            }],
        }
    }

    /// ADR-0004 decision 7.
    #[test]
    fn a_first_field_that_would_read_as_a_bom_is_quoted() {
        let bytes = "x,1\n\u{FEFF}y,2\n".as_bytes();
        let r = save(bytes, &[], &[Edit::DeleteRow { row: 0 }]).unwrap();
        assert_eq!(String::from_utf8(r.bytes).unwrap(), "\"\u{FEFF}y\",2\n");
        // Inserting a row with such a value at the start does the same.
        let ins = Edit::InsertRow {
            at: 0,
            values: vec!["\u{FEFF}z".into(), "0".into()],
        };
        let r = save(b"x,1\n", &[], &[ins]).unwrap();
        assert_eq!(
            String::from_utf8(r.bytes).unwrap(),
            "\"\u{FEFF}z\",0\nx,1\n"
        );
        // Elsewhere it is ordinary text.
        let r = save(bytes, &[], &[set(0, 1, "9")]).unwrap();
        assert_eq!(String::from_utf8(r.bytes).unwrap(), "x,9\n\u{FEFF}y,2\n");
    }

    /// ADR-0004 decision 11.
    #[test]
    fn an_encoding_hint_is_written_only_when_the_guess_would_differ() {
        let hint = |bytes: &[u8], enc, existing, edits: &[Edit]| {
            let layout = simple_layout(bytes, &[]);
            let mut doc =
                Document::new(bytes, &layout, Delimiter::Comma, enc).with_existing_hint(existing);
            for e in edits {
                doc.apply(e).unwrap();
            }
            doc.save().unwrap().encoding_hint
        };
        let w1252 = Encoding::Windows1252;
        // Clearing the only high byte leaves ASCII, which would reopen as
        // UTF-8, so the hint records Windows-1252.
        assert_eq!(
            hint(b"a\n\x80\n", w1252, None, &[set(1, 0, "")]),
            Some(w1252)
        );
        // Nothing would change: no hint.
        assert_eq!(hint(b"a\n\x80\n", w1252, None, &[set(0, 0, "b")]), None);
        assert_eq!(hint(b"a\n", Encoding::Utf8, None, &[set(0, 0, "é")]), None);
        // A file that already had a hint keeps (updates) it.
        assert_eq!(hint(b"a\n", w1252, Some(w1252), &[]), Some(w1252));
    }

    /// ADR-0005 decision 2: a hatched cell gets the delimiters needed to
    /// reach it, then its value, at the end of the row before its line
    /// ending, as one insert there (F2).
    #[test]
    fn a_hatched_cell_is_appended_to_its_row() {
        let r = save(b"a,b,c\n1\n2,3,4\n", &[], &[set(1, 2, "x")]).unwrap();
        assert_eq!(r.bytes, b"a,b,c\n1,,x\n2,3,4\n");
        assert_eq!(r.changes, vec![Change::insert(7, ",,x")]);
        // Next to an edit of the row's own field: one splice each.
        let r = save(b"a,b,c\n1\n", &[], &[set(1, 1, "y"), set(1, 0, "z")]).unwrap();
        assert_eq!(r.bytes, b"a,b,c\nz,y\n");
        assert_eq!(
            r.changes,
            vec![Change::replace(6..7, "z"), Change::insert(7, ",y")]
        );
        // The value is quoted as any edited value is.
        assert_eq!(
            out(save(b"a,b\n1\n", &[], &[set(1, 1, "x,y")])),
            "a,b\n1,\"x,y\"\n"
        );
        // With no line ending, at the very end.
        assert_eq!(out(save(b"a,b\n1", &[], &[set(1, 2, "x")])), "a,b\n1,,x");
        // A blank line edited in column c becomes a row of c + 1 fields.
        let r = save(b"a,b,c\n\nd\n", &[], &[set(1, 2, "x")]).unwrap();
        assert_eq!(r.bytes, b"a,b,c\n,,x\nd\n");
        assert_eq!(r.changes, vec![Change::insert(6, ",,x")]);
    }

    /// F3 for hatched cells: their value is empty, so `""` is no edit, and
    /// setting an edited one back to `""` removes it and its padding.
    #[test]
    fn a_hatched_cell_set_back_to_empty_is_no_edit() {
        let bytes = b"a,b,c,d\n1\n";
        let r = save(bytes, &[], &[set(1, 3, "")]).unwrap();
        assert!(r.changes.is_empty());
        let r = save(bytes, &[], &[set(1, 3, "x"), set(1, 3, "")]).unwrap();
        assert!(r.changes.is_empty());
        assert_eq!(r.bytes, bytes);
        // Padding up to a hatched cell that is still edited stays.
        let edits = [set(1, 1, "y"), set(1, 3, "x"), set(1, 3, "")];
        assert_eq!(out(save(bytes, &[], &edits)), "a,b,c,d\n1,y\n");
        let edits = [set(1, 3, "x"), set(1, 1, "y"), set(1, 1, "")];
        assert_eq!(out(save(bytes, &[], &edits)), "a,b,c,d\n1,,,x\n");
        // A blank line comes back blank.
        let r = save(b"a\n\n", &[], &[set(1, 2, "x"), set(1, 2, "")]).unwrap();
        assert_eq!(r.bytes, b"a\n\n");
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        assert_eq!(doc.value(1, 3), None);
        assert_eq!(doc.original_value(1, 3).as_deref(), Some(""));
        doc.apply(&set(1, 3, "x")).unwrap();
        assert_eq!(doc.row_len(1), 4);
        assert_eq!(doc.cell_source(1, 2), Some(CellSource::Padding));
        assert_eq!(doc.cell_source(1, 3), Some(CellSource::Edited));
        assert_eq!(doc.value(1, 2).as_deref(), Some(""));
        assert_eq!(doc.original_value(1, 3).as_deref(), Some(""));
    }

    /// Rule 12's limit: a hatched cell past [`COLUMN_LIMIT`] is refused.
    #[test]
    fn a_hatched_cell_past_the_column_limit_is_refused() {
        let bytes = b"a\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        let far = set(0, COLUMN_LIMIT, "x");
        assert_eq!(doc.apply(&far), Err(SaveError::InvalidEdit(far.clone())));
        doc.apply(&set(0, COLUMN_LIMIT - 1, "x")).unwrap();
        assert_eq!(doc.row_len(0), COLUMN_LIMIT);
    }

    #[test]
    fn invalid_edits_are_rejected() {
        let bytes = b"a\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        assert!(doc.apply(&set(1, 0, "x")).is_err());
        assert!(doc.apply(&Edit::DeleteRow { row: 1 }).is_err());
        assert!(
            doc.apply(&Edit::InsertRow {
                at: 0,
                values: vec![]
            })
            .is_err()
        );
        assert!(doc.apply(&Edit::DeleteColumn { column: 1 }).is_err());
        assert_eq!(doc.save().unwrap().bytes, b"a\n");
    }
}
