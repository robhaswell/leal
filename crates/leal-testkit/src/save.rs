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
//!    and the file's quoting style: every field quoted if the file quotes
//!    every field, otherwise only fields that need it.
//! 4. The trailing newline is kept as it was. If the file had none, the last
//!    row of the output has none, and a row that used to be last but no
//!    longer is gets the most common line ending.
//! 5. Setting a cell to its original display value removes the edit, so its
//!    original bytes come back (§3.6).
//! 6. Column insert and delete apply to every row that has that position:
//!    an insert at `c` needs at least `c` fields, a delete at `c` needs more
//!    than `c`. Shorter (ragged) rows are left alone.
//! 7. If an edited value can't be encoded, saving fails naming the cells
//!    (§3.7, F5). UTF-16 files are read-only in v1, so saving them fails.
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
//!     a column after it, or setting it back to its original value after
//!     something was added behind it. Editing it closes the quote.
//! 11. A lone CR directly followed by a blank LF row would read back as one
//!     CRLF, so the blank row's line ending becomes CR (ADR-0004
//!     decision 10). Only a blank row can start with LF.
//!
//! Rules 8, 9 and 11 are the "smallest extra change next to the edit" that
//! keeps ADR-0004 decision 10: reopening the saved file gives the same
//! rows, BOM and line endings. [`SavedFile::line_endings`] says which rows
//! a reopen must find. The encoding is kept by the encoding hint
//! ([`SavedFile::encoding_hint`], ADR-0004 decision 11).
//!
//! How these map to ADR-0004 (provisional until Rob accepts it): rule 3 is
//! decisions 1 and 3 (decision 2, per-column quoting, is not in yet: see
//! `TODO(ADR-0004 #2)`); rule 4 is decision 4; rule 5 is decision 9; rule 6
//! is decision 5; rules 8, 9 and 10 are decisions 6, 7 and 8; rules 9 and
//! 11 are decision 10.

use std::fmt;

use crate::dialect::{
    Delimiter, Encoding, LineEnding, UTF8_BOM, UTF16BE_BOM, UTF16LE_BOM, decode_value,
    encode_value, expected_encoding,
};
use crate::fidelity::{Change, apply_changes};
use crate::layout::Layout;

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
        /// Column, less than the row's field count.
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
    /// `com.apple.TextEncoding` extended attribute; reopen with
    /// [`crate::dialect::reopen_encoding`].
    pub encoding_hint: Option<Encoding>,
    /// The extra changes the save had to make to keep the file's structure
    /// (ADR-0004 decisions 6, 7 and 10), in the order they were made. Tests
    /// can compare these with the serializer's, and coverage tests use them
    /// to check that each rule is exercised.
    pub fixes: Vec<Fix>,
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

    /// The display value of a cell now.
    #[must_use]
    pub fn value(&self, row: usize, column: usize) -> Option<String> {
        let r = self.rows.get(row)?;
        Some(match r.cells.get(column)? {
            Cell::Original(f) => self.field_display(r.source?, *f),
            Cell::Edited { value, .. } => value.clone(),
        })
    }

    /// The original display value behind a cell, if it came from the file.
    #[must_use]
    pub fn original_value(&self, row: usize, column: usize) -> Option<String> {
        let r = self.rows.get(row)?;
        let field = match r.cells.get(column)? {
            Cell::Original(f) => Some(*f),
            Cell::Edited { field, .. } => *field,
        }?;
        Some(self.field_display(r.source?, field))
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
    /// Returns [`SaveError::InvalidEdit`] if its coordinates don't exist, and
    /// [`SaveError::AfterUnterminatedQuote`] if afterwards an original
    /// unterminated field would no longer be the last thing in the file, so
    /// bytes would land inside its quote (ADR-0004 decision 8). That covers
    /// inserting a row after its row, a column after it, and setting it back
    /// to its original value once something has been added after it.
    /// Inserting a row *at* its row index (before it) is allowed.
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

    fn apply_unchecked(&mut self, edit: &Edit) -> Result<(), SaveError> {
        let invalid = || SaveError::InvalidEdit(edit.clone());
        match edit {
            Edit::SetCell { row, column, value } => {
                let source = self.rows.get(*row).ok_or_else(invalid)?.source;
                let original = self.original_value(*row, *column);
                let cell = self
                    .rows
                    .get_mut(*row)
                    .and_then(|r| r.cells.get_mut(*column))
                    .ok_or_else(invalid)?;
                let field = match cell {
                    Cell::Original(f) => Some(*f),
                    Cell::Edited { field, .. } => *field,
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
                    // every column, so it never gains one.
                    if r.cells.len() >= *at && !r.is_blank_line(layout) {
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
        let quote_all = self.layout.quotes_every_field();
        let dominant = self.layout.line_endings().0.unwrap_or(LineEnding::Lf);
        let trailing_newline = self.layout.trailing_newline();

        // Each row's content bytes, and whether it is byte-for-byte original.
        let mut bad = Vec::new();
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
            if untouched {
                let span = self.layout.rows[row.source.unwrap_or(0)].span.clone();
                content.extend_from_slice(&self.bytes[span]);
            } else {
                for (ci, cell) in row.cells.iter().enumerate() {
                    if ci > 0 {
                        content.push(self.delimiter.byte());
                    }
                    match self.cell_bytes(row, cell, quote_all) {
                        Ok(b) => content.extend_from_slice(&b),
                        Err(()) => bad.push((ri, ci)),
                    }
                }
            }
            contents.push((content, untouched));
        }
        if !bad.is_empty() {
            return Err(SaveError::Unencodable(bad));
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
            && let Ok(first) = self.cell_bytes(row, cell, quote_all)
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
        let bytes: Vec<u8> = std::iter::once(&self.bytes[..self.layout.bom_len])
            .chain(serialized.iter().map(Vec::as_slice))
            .flatten()
            .copied()
            .collect();
        let changes = self.changes(&serialized, &contents, &endings);
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
        })
    }

    fn cell_bytes(&self, row: &DocRow, cell: &Cell, quote_all: bool) -> Result<Vec<u8>, ()> {
        match cell {
            Cell::Original(f) => {
                let s = row.source.ok_or(())?;
                Ok(self.bytes[self.layout.rows[s].fields[*f].span.clone()].to_vec())
            }
            Cell::Edited { field, value } => {
                // TODO(ADR-0004 #2): a new field (inserted row or column, so
                // `field` is `None`) should also be quoted when every existing
                // non-empty field in its column is quoted. Task 2.4 adds that
                // per-column check; for now only `quote_all` applies.
                let original_quoted = match (row.source, field) {
                    (Some(s), Some(f)) => self.layout.rows[s].fields[*f].quoted,
                    _ => false,
                };
                expected_field_bytes(
                    value,
                    self.encoding,
                    self.delimiter,
                    original_quoted,
                    quote_all,
                )
                .map_err(|_| ())
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
    /// edited field if only cells changed, otherwise the whole row.
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
        let same_shape = !untouched
            && ending == orig.line_ending
            && self.rows[j].cells.len() == orig.fields.len()
            && self.rows[j].cells.iter().enumerate().all(|(k, c)| match c {
                Cell::Original(f) => *f == k,
                Cell::Edited { field, .. } => *field == Some(k),
            });
        let end = orig.span.end + orig.line_ending.map_or(0, LineEnding::byte_len);
        if same_shape {
            let quote_all = self.layout.quotes_every_field();
            let mut per_field = Vec::new();
            for (k, cell) in self.rows[j].cells.iter().enumerate() {
                if let Cell::Edited { .. } = cell {
                    let bytes = self
                        .cell_bytes(&self.rows[j], cell, quote_all)
                        .unwrap_or_default();
                    let span = orig.fields[k].span.clone();
                    if bytes != self.bytes[span.clone()] {
                        per_field.push(Change::replace(span, bytes));
                    }
                }
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
        let bytes = b"x\n\xFF\xFEy\n";
        let layout = Layout {
            bom_len: 0,
            rows: vec![
                row_layout(0..1, LineEnding::Lf, b"x"),
                row_layout(2..5, LineEnding::Lf, b"\xFF\xFEy"),
            ],
        };
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Windows1252);
        doc.apply(&Edit::DeleteRow { row: 0 }).unwrap();
        assert_eq!(doc.save().unwrap().bytes, b"\"\xFF\xFEy\"\n");
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

    #[test]
    fn invalid_edits_are_rejected() {
        let bytes = b"a\n";
        let layout = simple_layout(bytes, &[]);
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
        assert!(doc.apply(&set(0, 1, "x")).is_err());
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
