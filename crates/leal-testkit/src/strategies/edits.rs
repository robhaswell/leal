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

use crate::dialect::{Delimiter, Encoding, LineEnding};
use crate::save::{Edit, SaveError, SavedFile};
use crate::strategies::csv::{
    CsvConfig, CsvModel, GeneratedCsv, LineEndings, ModelDialect, ModelField, ModelRow,
    QuotingStyle, csv_file,
};

/// Values the edit strategy writes: plain text, every character that forces
/// quoting, leading and trailing spaces, the empty string, text that
/// Windows-1252 can (é, €) and can't (😀) encode, and BOM-like starts
/// (ADR-0004 decisions 7 and 10): U+FEFF, and "ÿþ" and "ï»¿", which are the
/// bytes `FF FE` and `EF BB BF` in Windows-1252.
pub const EDIT_VALUES: [&str; 21] = [
    "\u{FEFF}x",
    "\u{FF}\u{FE}",
    "\u{EF}\u{BB}\u{BF}",
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
    /// The encoding hint the file had when opened (ADR-0004 decision 11):
    /// sometimes `Some(file.encoding)`, as if an earlier save had written it.
    /// Replay with `file.document().with_existing_hint(existing_hint)`.
    pub existing_hint: Option<Encoding>,
    /// The edits, in order, in logical coordinates. Every one is valid when
    /// applied in turn.
    pub edits: Vec<Edit>,
    /// The expected save: the exact output bytes and the splices from the
    /// original, or why saving must fail (an unencodable value, F5, or a
    /// read-only UTF-16 file).
    pub saved: Result<SavedFile, SaveError>,
}

impl fmt::Debug for EditCase {
    // Proptest prints this for a failing case, so it shows every input needed
    // to rebuild the case by hand (including `existing_hint`) and the parts of
    // the expected save that tests compare. The splices and line endings are
    // left out: they follow from the bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let saved = match &self.saved {
            Ok(s) => format!(
                "Ok(b\"{}\", encoding_hint: {:?}, fixes: {:?})",
                s.bytes.escape_ascii(),
                s.encoding_hint,
                s.fixes
            ),
            Err(e) => format!("Err({e:?})"),
        };
        f.debug_struct("EditCase")
            .field("file", &self.file)
            .field("existing_hint", &self.existing_hint)
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
    /// Set the file's first cell, often to a BOM-like value, so that
    /// ADR-0004 decisions 7 and 10 (quoting BOM-like first fields) fire.
    SetFirst {
        value: RawValue,
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
        1 => prop_oneof![
            select(&EDIT_VALUES[..3]).prop_map(RawValue::Literal), // the BOM-like values
            raw_value(),
        ]
        .prop_map(|value| RawEdit::SetFirst { value }),
        1 => (any::<Index>(), [raw_value(), raw_value(), raw_value(), raw_value()])
            .prop_map(|(at, values)| RawEdit::InsertRow { at, values }),
        1 => any::<Index>().prop_map(|row| RawEdit::DeleteRow { row }),
        1 => (any::<Index>(), raw_value()).prop_map(|(at, value)| RawEdit::InsertColumn { at, value }),
        1 => any::<Index>().prop_map(|column| RawEdit::DeleteColumn { column }),
    ]
}

/// Edit cases over files from [`csv_file`]`(config)`: up to 6 edits each.
///
/// If `config` allows mixed line endings and blank lines, three files in ten
/// is instead a run of `x CR`, `y LF`, blank `LF` rows, so that deleting a
/// `y` row puts a lone CR before a blank LF row (the CR/LF split, ADR-0004
/// decision 10) often enough to test.
pub fn edit_case(config: CsvConfig) -> impl Strategy<Value = EditCase> {
    let m = config.messiness;
    let files = if m.mixed_line_endings && m.blank_lines {
        prop_oneof![7 => csv_file(config), 3 => cr_then_blank_lf_file()].boxed()
    } else {
        csv_file(config).boxed()
    };
    edits_for(files, 6)
}

/// Blocks of `x CR`, `y LF`, blank `LF`, one to four times.
fn cr_then_blank_lf_file() -> impl Strategy<Value = GeneratedCsv> {
    (
        select(&Delimiter::ALL[..]),
        vec(select(&["a", "b", "c", "long value"][..]), 1..=4),
    )
        .prop_map(|(delimiter, words)| {
            let row = |v: &str, le| ModelRow {
                fields: vec![ModelField::Unquoted(v.as_bytes().to_vec())],
                line_ending: Some(le),
            };
            let rows = words
                .iter()
                .flat_map(|w| {
                    [
                        row(w, LineEnding::Cr),
                        row(w, LineEnding::Lf),
                        row("", LineEnding::Lf),
                    ]
                })
                .collect();
            let dialect = ModelDialect {
                delimiter,
                line_endings: LineEndings::Mixed,
                bom: false,
                quoting: QuotingStyle::Minimal,
            };
            GeneratedCsv::from_model(CsvModel { dialect, rows })
        })
}

/// Edit cases over files from any strategy (for example
/// `csv_file_utf16`), with up to `max_edits` raw edits (a reverted cell
/// edit counts once but produces two edits).
pub fn edits_for(
    files: impl Strategy<Value = GeneratedCsv>,
    max_edits: usize,
) -> impl Strategy<Value = EditCase> {
    (
        files,
        vec(raw_edit(), 0..=max_edits),
        prop::bool::weighted(0.2),
    )
        .prop_map(|(file, raw, hinted)| resolve(file, &raw, hinted))
}

/// Resolves raw edits against the document as it evolves, and saves.
/// `hinted` gives the file an existing encoding hint, where one can exist.
fn resolve(file: GeneratedCsv, raw: &[RawEdit], hinted: bool) -> EditCase {
    let existing_hint = (hinted && matches!(file.encoding, Encoding::Utf8 | Encoding::Windows1252))
        .then_some(file.encoding);
    let mut doc = file.document().with_existing_hint(existing_hint);
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
            RawEdit::SetFirst { value } => {
                if doc.row_len(0) == 0 {
                    continue;
                }
                let value = match value {
                    RawValue::Literal(s) => s.to_owned(),
                    RawValue::Original => doc.original_value(0, 0).unwrap_or_default(),
                };
                step.push(Edit::SetCell {
                    row: 0,
                    column: 0,
                    value,
                });
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
    EditCase {
        file,
        existing_hint,
        edits,
        saved,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `a,b` LF, with `edits` applied and saved.
    fn case(existing_hint: Option<Encoding>, edits: Vec<Edit>) -> EditCase {
        let file = GeneratedCsv::from_model(CsvModel {
            dialect: ModelDialect {
                delimiter: Delimiter::Comma,
                line_endings: LineEndings::Uniform(LineEnding::Lf),
                bom: false,
                quoting: QuotingStyle::Minimal,
            },
            rows: vec![ModelRow {
                fields: vec![
                    ModelField::Unquoted(b"a".to_vec()),
                    ModelField::Unquoted(b"b".to_vec()),
                ],
                line_ending: Some(LineEnding::Lf),
            }],
        });
        let mut doc = file.document().with_existing_hint(existing_hint);
        for e in &edits {
            doc.apply(e).unwrap();
        }
        let saved = doc.save();
        EditCase {
            file,
            existing_hint,
            edits,
            saved,
        }
    }

    /// Proptest prints this for a shrunk failure, and it is the only
    /// human-readable record of one, so it must show every input.
    #[test]
    fn debug_shows_every_input_and_the_expected_save() {
        let case = case(
            Some(Encoding::Utf8),
            vec![Edit::SetCell {
                row: 0,
                column: 0,
                value: "\u{FEFF}x".to_owned(),
            }],
        );
        assert_eq!(
            format!("{case:?}"),
            concat!(
                r#"EditCase { file: GeneratedCsv { bytes: b"a,b\n", dialect: ModelDialect { "#,
                r#"delimiter: Comma, line_endings: Uniform(Lf), bom: false, quoting: Minimal }, "#,
                r#"encoding: Utf8, rows: [ModelRow { fields: [Unquoted("a"), Unquoted("b")], "#,
                r#"line_ending: Some(Lf) }], diagnostics: [] }, "#,
                r#"existing_hint: Some(Utf8), "#,
                r#"edits: [SetCell { row: 0, column: 0, value: "\u{feff}x" }], "#,
                r#"saved: Ok(b"\"\xef\xbb\xbfx\",b\n", encoding_hint: Some(Utf8), "#,
                r#"fixes: [BomLikeQuoted]) }"#,
            )
        );
    }

    #[test]
    fn debug_shows_a_missing_hint_and_a_failed_save() {
        let mut case = case(None, Vec::new());
        case.saved = Err(SaveError::ReadOnly);
        let printed = format!("{case:?}");
        assert!(printed.contains(" existing_hint: None, "), "{printed}");
        assert!(printed.ends_with(" saved: Err(ReadOnly) }"), "{printed}");
    }
}
