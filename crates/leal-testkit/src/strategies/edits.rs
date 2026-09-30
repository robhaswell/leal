//! Edits for fidelity tests (F2, F3, F5, F6): a generated file, a sequence of
//! concrete [`Edit`]s, and the expected result of saving, from the save
//! oracle in [`crate::save`].
//!
//! ```
//! use leal_testkit::fidelity::check_only_changed;
//! use leal_testkit::strategies::csv::CsvConfig;
//! use leal_testkit::strategies::edits::edit_case;
//! use proptest::prelude::*;
//!
//! proptest! {
//!     // In a test file, put `#[test]` on the line above `fn`.
//!     fn saving_changes_only_the_edited_bytes(case in edit_case(CsvConfig::messy())) {
//!         // A real test replays `case.edits` on the real document and saves.
//!         // Here the oracle's own output stands in for the serializer's.
//!         if let Ok(saved) = &case.saved {
//!             check_only_changed(&case.file.bytes, &saved.bytes, &saved.changes)?;
//!         }
//!     }
//! }
//! # saving_changes_only_the_edited_bytes();
//! ```

use std::fmt;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::{Index, select};

use crate::save::{Edit, SaveError, SavedFile};
use crate::strategies::csv::{CsvConfig, GeneratedCsv, csv_file};

/// Values the edit strategy writes: plain text, every character that forces
/// quoting, leading and trailing spaces, the empty string, and text that
/// Windows-1252 can (é, €) and can't (😀) encode.
pub const EDIT_VALUES: [&str; 18] = [
    "",
    "x",
    "new value",
    "42",
    "a,b",
    "a;b",
    "a\tb",
    "a|b",
    "say \"hi\"",
    "\"",
    "\"lead",
    "line\nbreak",
    "cr\rhere",
    "crlf\r\nhere",
    " padded ",
    "é",
    "€",
    "😀",
];

/// A file, the edits made to it, and what saving must produce.
#[derive(Clone)]
pub struct EditCase {
    /// The original file.
    pub file: GeneratedCsv,
    /// The edits, in order, in logical coordinates. Every one is valid when
    /// applied in turn.
    pub edits: Vec<Edit>,
    /// The expected save: the exact output bytes and the splices from the
    /// original, or why saving must fail (an unencodable value, F5, or a
    /// read-only UTF-16 file).
    pub saved: Result<SavedFile, SaveError>,
}

impl fmt::Debug for EditCase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let saved = match &self.saved {
            Ok(s) => format!("Ok(b\"{}\")", s.bytes.escape_ascii()),
            Err(e) => format!("Err({e:?})"),
        };
        f.debug_struct("EditCase")
            .field("file", &self.file)
            .field("edits", &self.edits)
            .field("saved", &format_args!("{saved}"))
            .finish()
    }
}

/// A value for an edit: one of [`EDIT_VALUES`], or the cell's original
/// display value (so that some edits set a cell back to what it was).
#[derive(Clone, Copy, Debug)]
enum RawValue {
    Literal(&'static str),
    Original,
}

/// An edit before its coordinates are resolved against the document.
#[derive(Clone, Debug)]
enum RawEdit {
    Set {
        row: Index,
        column: Index,
        value: RawValue,
        /// Also set the cell back to its original value afterwards (F3).
        revert: bool,
    },
    InsertRow {
        at: Index,
        values: [RawValue; 4],
    },
    DeleteRow {
        row: Index,
    },
    InsertColumn {
        at: Index,
        value: RawValue,
    },
    DeleteColumn {
        column: Index,
    },
}

fn raw_value() -> impl Strategy<Value = RawValue> {
    prop_oneof![
        6 => select(&EDIT_VALUES[..]).prop_map(RawValue::Literal),
        1 => Just(RawValue::Original),
    ]
}

fn raw_edit() -> impl Strategy<Value = RawEdit> {
    prop_oneof![
        6 => (any::<Index>(), any::<Index>(), raw_value(), prop::bool::weighted(0.25))
            .prop_map(|(row, column, value, revert)| RawEdit::Set { row, column, value, revert }),
        1 => (any::<Index>(), [raw_value(), raw_value(), raw_value(), raw_value()])
            .prop_map(|(at, values)| RawEdit::InsertRow { at, values }),
        1 => any::<Index>().prop_map(|row| RawEdit::DeleteRow { row }),
        1 => (any::<Index>(), raw_value()).prop_map(|(at, value)| RawEdit::InsertColumn { at, value }),
        1 => any::<Index>().prop_map(|column| RawEdit::DeleteColumn { column }),
    ]
}

/// Edit cases over files from [`csv_file`]`(config)`: up to 6 edits each.
pub fn edit_case(config: CsvConfig) -> impl Strategy<Value = EditCase> {
    edits_for(csv_file(config), 6)
}

/// Edit cases over files from any strategy (for example
/// `csv_file_utf16`), with up to `max_edits` raw edits (a reverted cell
/// edit counts once but produces two edits).
pub fn edits_for(
    files: impl Strategy<Value = GeneratedCsv>,
    max_edits: usize,
) -> impl Strategy<Value = EditCase> {
    (files, vec(raw_edit(), 0..=max_edits)).prop_map(|(file, raw)| resolve(file, &raw))
}

/// Resolves raw edits against the document as it evolves, and saves.
fn resolve(file: GeneratedCsv, raw: &[RawEdit]) -> EditCase {
    let mut doc = file.document();
    let mut edits = Vec::new();
    for r in raw {
        let mut step = Vec::new();
        match *r {
            RawEdit::Set {
                row,
                column,
                value,
                revert,
            } => {
                if doc.row_count() == 0 {
                    continue;
                }
                let row = row.index(doc.row_count());
                let len = doc.row_len(row);
                if len == 0 {
                    continue;
                }
                let column = column.index(len);
                let original = doc.original_value(row, column);
                let value = match value {
                    RawValue::Literal(s) => s.to_owned(),
                    RawValue::Original => original.clone().unwrap_or_default(),
                };
                step.push(Edit::SetCell { row, column, value });
                if let (true, Some(original)) = (revert, original) {
                    step.push(Edit::SetCell {
                        row,
                        column,
                        value: original,
                    });
                }
            }
            RawEdit::InsertRow { at, values } => {
                let at = at.index(doc.row_count() + 1);
                let n = doc.typical_row_len();
                let values = (0..n)
                    .map(|i| match values[i % values.len()] {
                        RawValue::Literal(s) => s.to_owned(),
                        RawValue::Original => String::new(),
                    })
                    .collect();
                step.push(Edit::InsertRow { at, values });
            }
            RawEdit::DeleteRow { row } => {
                if doc.row_count() > 0 {
                    step.push(Edit::DeleteRow {
                        row: row.index(doc.row_count()),
                    });
                }
            }
            RawEdit::InsertColumn { at, value } => {
                let at = at.index(doc.max_row_len() + 1);
                let value = match value {
                    RawValue::Literal(s) => s.to_owned(),
                    RawValue::Original => String::new(),
                };
                step.push(Edit::InsertColumn { at, value });
            }
            RawEdit::DeleteColumn { column } => {
                if doc.max_row_len() > 0 {
                    step.push(Edit::DeleteColumn {
                        column: column.index(doc.max_row_len()),
                    });
                }
            }
        }
        for e in step {
            // Coordinates were resolved against `doc`, so this cannot fail.
            if doc.apply(&e).is_ok() {
                edits.push(e);
            }
        }
    }
    let saved = doc.save();
    EditCase { file, edits, saved }
}
