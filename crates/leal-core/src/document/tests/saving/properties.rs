//! The save properties (tasks 2.2 and 2.3, DESIGN §5 F1–F5): the
//! testkit's edit cases replayed on the real document and saved for real,
//! against the save oracle (`leal_testkit::save::Document`), which never
//! sees leal-core's code; then the saved file reopened.
//!
//! - **The same save.** The same bytes on disk, the same splices
//!   ([`SavePlan::splices`] against `SavedFile::changes`, so F2 is checked
//!   splice by splice, hatched cells included), the same fixes, the same
//!   encoding hint (`com.apple.TextEncoding`, read back from the file) and
//!   the same line endings, row by row, as the saved file reads. Or the
//!   same refusal: the oracle and the save name the same cells, and the
//!   file is untouched (F5).
//! - **Every encoding** (task 2.3, ADR-0005 decision 5): files in UTF-8,
//!   in each single-byte encoding (edited with that encoding's own
//!   characters, so they round-trip byte for byte, and with characters it
//!   doesn't have, so they are refused), and in UTF-16, which is read-only.
//! - **The reopen property** (ADR-0004 decision 10, narrowed by ADR-0005
//!   decision 1): opening the saved file afresh, with the attributes the
//!   save wrote, gives the same BOM, encoding, delimiter and header choice,
//!   every row's values and line endings, no note about the attributes, and
//!   no whole-file review suggestion (phase 1 review). A single-byte
//!   encoding's attribute Leal wrote holds even if a byte doesn't decode in
//!   it, since Leal marks it as its own (ADR-0013 decision 2;
//!   `tk::reopen_encoding` with `HintWriter::Leal`).
//! - **The rebase** (ADR-0008 decision 1): the document reads the saved
//!   file afterwards, with no edits left, in the same lineage.
//! - **Save As UTF-8** (ADR-0008 decision 7): the bytes the oracle's
//!   `save_as_utf8` gives, the attribute set to UTF-8, the BOM a UTF-8 one
//!   exactly when the file had one; the document then reads the new file in
//!   UTF-8 with the same values, and so does a reopen, with no suggestion.
//!   Or the cells that can't be converted, named as the oracle names them,
//!   and nothing written.
//!
//! - **Rows and columns inserted and deleted** (task 2.4c, F6): every edit
//!   the strategy made (cells, rows, columns) is replayed, some undone at
//!   once and some of those redone, and saved splice for splice; the new
//!   reading's index, built from the save's plan, is the saved file's.
//! - **Undo after a save** (ADR-0012 decision 4, ADR-0014 decision 3): every
//!   command undone afterwards, by value, saves as the oracle saves the
//!   same undos made on the saved file: a column insert's undo deletes the
//!   cells it gave, a column delete's puts its cells back (`Restore`).
//! - **Save As from an incomplete document** (ADR-0008 decision 6): a copy
//!   that stops part way, with the edits the document then takes, writes
//!   the oracle's rows up to the cut.
//!
//! The case's existing encoding hint is passed through as a real
//! `com.apple.TextEncoding` on the file before it is opened.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use leal_testkit::dialect as tk;
use leal_testkit::fidelity::{Change, check_identical, check_only_changed};
use leal_testkit::save::{
    Document as Oracle, Edit as OracleEdit, Fix as OracleFix, SaveError as OracleError, SavedFile,
};
use leal_testkit::strategies::csv::{CsvConfig, csv_file_utf16};
use leal_testkit::strategies::edits::{EditCase, edit_case, edits_for, single_byte_edit_case};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{RngAlgorithm, TestCaseError, TestRng, TestRunner};

use super::*;
use crate::attributes::text_encoding_value;
use crate::detect::{Choices, Hints, detect};
use crate::dialect::Bom;
use crate::edit::EditError;
use crate::save::{Fix, SaveError, SaveKind, SaveRequest};
use crate::source::tests::{attribute, write_attribute};
use crate::source::{
    INTERPRETATION_ATTRIBUTE_C, SimulatedFault, Source, TEXT_ENCODING_ATTRIBUTE_C,
};

/// Many documents are opened, two per case: one scheduler for all.
static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(scheduler);

static DIR: LazyLock<Dir> = LazyLock::new(|| Dir::new("save-properties"));

fn core_encoding(e: tk::Encoding) -> Encoding {
    Encoding::ALL
        .into_iter()
        .find(|ours| ours.iana_name() == e.name())
        .unwrap()
}

fn tk_encoding(e: Encoding) -> Option<tk::Encoding> {
    [
        tk::Encoding::Utf8,
        tk::Encoding::Utf16Le,
        tk::Encoding::Utf16Be,
    ]
    .into_iter()
    .chain(tk::Encoding::SINGLE_BYTE)
    .find(|theirs| theirs.name() == e.iana_name())
}

fn tk_line_ending(l: crate::dialect::LineEnding) -> tk::LineEnding {
    match l {
        crate::dialect::LineEnding::Lf => tk::LineEnding::Lf,
        crate::dialect::LineEnding::Crlf => tk::LineEnding::Crlf,
        crate::dialect::LineEnding::Cr => tk::LineEnding::Cr,
    }
}

fn tk_fix(fix: Fix) -> OracleFix {
    match fix {
        Fix::EmptyRowQuoted { row } => OracleFix::EmptyRowQuoted { row },
        Fix::CrSplit { row } => OracleFix::CrSplit { row },
        Fix::BomLikeQuoted => OracleFix::BomLikeQuoted,
    }
}

/// Writes `case`'s file, with its existing encoding hint as a real
/// `com.apple.TextEncoding`, and opens it as a user would: the delimiter
/// guessed if detection finds the file's own, otherwise chosen; the
/// encoding detected (from the attribute, or guessed), unless detection
/// gives another (a large file whose first 64 KB guess otherwise, or an
/// encoding detection never picks, whose attribute doesn't hold or isn't
/// there), when it is chosen; the header detected. Returns the document,
/// its path, and whether the encoding was chosen.
fn open_case(case: &EditCase) -> (Arc<Document>, PathBuf, bool) {
    let (path, options, chosen_encoding) = write_case(case);
    let (document, _) = Document::open(
        &path,
        &DIR.temp(),
        VolumeInfo::default(),
        &SCHEDULER,
        options,
        None,
    )
    .unwrap();
    wait_for_index(&document);
    (Arc::new(document), path, chosen_encoding)
}

/// Writes `case`'s file, with its existing hint as an attribute: its path,
/// the options that open it as the case has it, and whether the encoding
/// is chosen.
fn write_case(case: &EditCase) -> (PathBuf, OpenOptions, bool) {
    static FILES: AtomicU64 = AtomicU64::new(0);
    let name = format!("{}.csv", FILES.fetch_add(1, Ordering::Relaxed));
    let path = DIR.file(&name, &case.file.bytes);
    let hint = case
        .existing_hint
        .map(|e| text_encoding_value(core_encoding(e)).into_bytes());
    if let Some(value) = &hint {
        write_attribute(&path, TEXT_ENCODING_ATTRIBUTE_C, Some(value));
    }
    let bytes = &case.file.bytes;
    let hints = Hints {
        text_encoding: hint.as_deref(),
        interpretation: None,
    };
    let len = u64::try_from(bytes.len()).unwrap();
    let guess = detect(bytes, len, hints, Choices::default()).unwrap();
    let delimiter = Delimiter::from_byte(case.file.delimiter().byte()).unwrap();
    let encoding = core_encoding(case.file.encoding);
    let chosen_encoding = guess.encoding != encoding;
    let choices = Choices {
        delimiter: (guess.delimiter != delimiter).then_some(delimiter),
        encoding: chosen_encoding.then_some(encoding),
        header: None,
    };
    let options = OpenOptions {
        choices,
        ..options(10)
    };
    (path, options, chosen_encoding)
}

fn set(row: usize, column: usize, value: &str) -> OracleEdit {
    OracleEdit::SetCell {
        row,
        column,
        value: value.to_owned(),
    }
}

/// The case's edits, in the order the strategy tried them, the ones the
/// oracle refused (ADR-0004 decision 8) included at their places.
fn case_edits(case: &EditCase) -> Vec<OracleEdit> {
    let mut all = Vec::new();
    let mut refused = case.refused.iter().peekable();
    for (at, edit) in case.edits.iter().enumerate() {
        while let Some((_, edit)) = refused.next_if(|(place, _)| *place == at) {
            all.push(edit.clone());
        }
        all.push(edit.clone());
    }
    all.extend(refused.map(|(_, edit)| edit.clone()));
    all
}

/// An oracle edit that undoes a command by value, on the file a save
/// wrote (ADR-0014 decision 3).
#[derive(Clone, Debug)]
enum Undo {
    Edit(OracleEdit),
    /// A column insert's: its cells deleted from the rows it gave them.
    DeleteColumnFrom {
        column: usize,
        rows: Vec<usize>,
    },
    /// A column delete's: its cells put back.
    RestoreColumn {
        at: usize,
        cells: Vec<(usize, String)>,
    },
}

impl Undo {
    fn apply(&self, oracle: &mut Oracle<'_>) -> Result<(), OracleError> {
        match self {
            Undo::Edit(edit) => oracle.apply(edit),
            Undo::DeleteColumnFrom { column, rows } => oracle.delete_column_from(*column, rows),
            Undo::RestoreColumn { at, cells } => oracle.restore_column(*at, cells),
        }
    }
}

/// A step that must succeed, failed.
fn fail(error: impl std::fmt::Debug) -> TestCaseError {
    TestCaseError::fail(format!("{error:?}"))
}

/// Whether the case's `n`th command is undone at once, and redone after:
/// a sixth of them undone, half of those redone.
fn undone(case: &EditCase, n: usize) -> (bool, bool) {
    match (n * 7 + case.file.bytes.len()) % 12 {
        0 | 1 => (true, false),
        2 => (true, true),
        _ => (false, false),
    }
}

/// A command the document made, and the oracle edits that undo it by
/// value, on the file a save wrote.
struct Made {
    command: Command,
    undo: Vec<Undo>,
}

/// Opens `case`'s file ([`open_case`]) and replays its edits on the
/// document and on the oracle, which must accept and refuse the same.
#[expect(clippy::type_complexity, reason = "what a case starts from")]
fn open_and_edit(
    case: &EditCase,
) -> Result<(Arc<Document>, PathBuf, bool, Oracle<'_>, Vec<Made>), TestCaseError> {
    let (document, opened, chosen_encoding) = open_case(case);
    let mut oracle = case.file.document().with_existing_hint(case.existing_hint);
    let mut redone = 0_u16;
    let mut made = Vec::new();
    for edit in case_edits(case) {
        let before = oracle.clone();
        let expected = oracle.apply(&edit);
        // The rows whose length the edit changed: a column operation's.
        let changed = |longer: bool| -> Vec<usize> {
            (0..before.row_count())
                .filter(|&row| {
                    let (was, now) = (before.row_len(row), oracle.row_len(row));
                    if longer { now > was } else { now < was }
                })
                .collect()
        };
        let (got, undo) = match &edit {
            OracleEdit::SetCell { row, column, value } => {
                let old = before.value(*row, *column).unwrap_or_default();
                (
                    document.set_cell(*row, *column, value),
                    vec![Undo::Edit(set(*row, *column, &old))],
                )
            }
            OracleEdit::InsertRow { at, values } => (
                document.insert_rows(*at, std::slice::from_ref(values)),
                vec![Undo::Edit(OracleEdit::DeleteRow { row: *at })],
            ),
            OracleEdit::DeleteRow { row } => {
                let values: Vec<String> = (0..before.row_len(*row))
                    .map(|column| before.value(*row, column).unwrap_or_default())
                    .collect();
                // A row column deletes emptied comes back with no cells
                // (ADR-0014 decision 6), which the oracle makes in two.
                let undo = if values.is_empty() {
                    vec![
                        Undo::Edit(OracleEdit::InsertRow {
                            at: *row,
                            values: vec![String::new()],
                        }),
                        Undo::DeleteColumnFrom {
                            column: 0,
                            rows: vec![*row],
                        },
                    ]
                } else {
                    vec![Undo::Edit(OracleEdit::InsertRow { at: *row, values })]
                };
                (document.delete_rows(*row, 1), undo)
            }
            OracleEdit::InsertColumn { at, value } => (
                document.insert_column(*at, value),
                vec![Undo::DeleteColumnFrom {
                    column: *at,
                    rows: changed(true),
                }],
            ),
            OracleEdit::DeleteColumn { column } => {
                let cells = changed(false)
                    .into_iter()
                    .map(|row| (row, before.value(row, *column).unwrap_or_default()))
                    .collect();
                (
                    document.delete_column(*column),
                    vec![Undo::RestoreColumn { at: *column, cells }],
                )
            }
        };
        match (&expected, got) {
            (Ok(()), Ok(Some(command))) => {
                // In the session, an undo works by identity, and a redo
                // makes the same change again.
                let (undo_now, redo) = undone(case, made.len() + usize::from(redone));
                if undo_now {
                    document.apply(&command.inverse()).map_err(fail)?;
                    oracle = before.clone();
                    if !redo {
                        redone += 1;
                        continue;
                    }
                    document.apply(&command).map_err(fail)?;
                    oracle.apply(&edit).map_err(fail)?;
                    redone += 1;
                }
                made.push(Made { command, undo });
            }
            (Ok(()), Ok(None))
            | (
                Err(OracleError::InvalidEdit(_)),
                Err(EditError::NoSuchRow { .. } | EditError::NoSuchColumn { .. }),
            )
            | (
                Err(OracleError::AfterUnterminatedQuote(_)),
                Err(EditError::AfterUnterminatedQuote { .. }),
            ) => {}
            (_, got) => prop_assert!(
                false,
                "{:?}: oracle {:?}, document {:?}",
                edit,
                expected,
                got
            ),
        }
    }
    Ok((document, opened, chosen_encoding, oracle, made))
}

/// Undoes every command in `made` after the save that wrote `saved`, by
/// value, and the same undos on an oracle of the saved file: the next
/// save is the same, splice for splice (ADR-0012 decision 4, ADR-0014
/// decision 3). Returns whether a row command was undone.
fn check_undo_after_save(
    document: &Document,
    case: &EditCase,
    saved: &SavedFile,
    made: &[Made],
) -> Result<bool, TestCaseError> {
    let layout = saved.layout.as_ref().unwrap();
    let mut oracle = Oracle::new(
        &saved.bytes,
        layout,
        case.file.delimiter(),
        case.file.encoding,
    );
    for made in made.iter().rev() {
        let got = document.apply(&made.command.inverse());
        let expected = made
            .undo
            .iter()
            .try_for_each(|undo| undo.apply(&mut oracle));
        match (&expected, &got) {
            (Ok(()), Ok(_)) => {}
            _ => prop_assert!(
                false,
                "undo {:?}: oracle {:?}, document {:?}",
                made.undo,
                expected,
                got
            ),
        }
    }
    let plan = document.save_plan(SaveKind::Save);
    match (oracle.save(), plan) {
        (Ok(expected), Ok(plan)) => {
            let splices: Vec<Change> = plan
                .splices()
                .iter()
                .map(|s| Change::replace(s.range.clone(), s.bytes.clone()))
                .collect();
            prop_assert_eq!(&splices, &expected.changes);
            let fixes: Vec<OracleFix> = plan.fixes().iter().copied().map(tk_fix).collect();
            prop_assert_eq!(&fixes, &expected.fixes);
            prop_assert_eq!(plan.rows(), oracle.row_count());
        }
        (Err(OracleError::Unencodable(cells)), Err(SaveError::Unencodable { cells: got, .. })) => {
            prop_assert_eq!(got, cells);
        }
        (expected, plan) => prop_assert!(false, "oracle {:?}, plan {:?}", expected, plan),
    }
    Ok(made
        .iter()
        .any(|made| made.command.is_structural() && !made.command.is_column()))
}

/// How a case is saved: Save over the file; Save over it after another
/// app rewrote it unchanged (a new modification time), with the user's
/// agreement to write over it; or Save As to a new place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Save,
    Overwrite,
    SaveAs,
}

fn modes() -> impl Strategy<Value = Mode> {
    prop_oneof![
        3 => Just(Mode::Save),
        1 => Just(Mode::Overwrite),
        1 => Just(Mode::SaveAs),
    ]
}

/// What a compared case exercised, for the coverage test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Covered {
    /// An insert at a row's end: a hatched cell.
    hatched: bool,
    /// A fix (ADR-0004 decisions 6, 7, 10).
    fixed: bool,
    /// A file that quotes every field, with a cell edited that has no
    /// quoting of its own (a hatched cell, or a blank line's).
    quote_all: bool,
    /// A single-byte file whose save wrote bytes 0x80–0xFF in a splice:
    /// characters other than ASCII, in that encoding.
    high_bytes: bool,
    /// Rows inserted or deleted, saved, and undone after the save.
    rows: bool,
    /// Columns inserted or deleted, saved, and undone after the save.
    columns: bool,
}

/// How a case went, for the coverage test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Saved, and compared with the oracle in full.
    Compared(Covered),
    /// Refused as read-only (UTF-16), as the oracle refuses it.
    ReadOnly,
    /// Refused, naming the cells the oracle names: a character the
    /// encoding can't represent (F5).
    Unencodable,
}

/// Replays `case`'s cell edits on the document and on the oracle, then
/// saves both (see the module docs).
fn save_matches_the_oracle(case: &EditCase, mode: Mode) -> Result<Outcome, TestCaseError> {
    // Tiny write chunks (61 bytes, prime), so the writer's copies between
    // splices cross chunk boundaries everywhere, inside rows and fields.
    crate::document::saving::TEST_CHUNK_BYTES.store(61, Ordering::Relaxed);
    let (document, opened, chosen_encoding, oracle, made) = open_and_edit(case)?;
    let lineage = document.lineage();
    let expected = oracle.save();
    let kind = if mode == Mode::SaveAs {
        SaveKind::SaveAs
    } else {
        SaveKind::Save
    };
    let path = match mode {
        Mode::SaveAs => opened.with_extension("as.csv"),
        Mode::Save | Mode::Overwrite => opened.clone(),
    };
    let mut request = SaveRequest::new(&path, kind);
    if mode == Mode::Overwrite {
        // Another app rewrites it, the same bytes, a moment later.
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&opened, &case.file.bytes).unwrap();
        let refused = document.save(SaveRequest::new(&path, kind));
        if !matches!(
            refused.wait(),
            Err(SaveError::ReadOnly | SaveError::Unencodable { .. })
        ) {
            prop_assert!(
                matches!(refused.wait(), Err(SaveError::ChangedElsewhere)),
                "{:?}",
                refused.wait()
            );
        }
        request.overwrite_changed = true;
    }
    let plan = document.save_plan(kind);
    let job = document.save(request);
    let result = job.wait();
    let on_disk = std::fs::read(&path).unwrap_or_default();
    if mode == Mode::SaveAs {
        prop_assert_eq!(&std::fs::read(&opened).unwrap(), &case.file.bytes);
    }
    let encoding = document.detection().encoding;

    let mut covered;
    let saved = match (expected, plan, result) {
        (Ok(saved), Ok(plan), Ok(done)) => {
            covered = Covered {
                hatched: plan.splices().iter().any(|s| {
                    s.range.is_empty() && s.bytes.first() == Some(&case.file.delimiter().byte())
                }),
                fixed: !done.fixes.is_empty(),
                quote_all: case.file.layout.quotes_every_field()
                    && plan.splices().iter().any(|s| {
                        s.range.is_empty()
                            || case
                                .file
                                .layout
                                .rows
                                .iter()
                                .any(|r| r.is_blank() && r.span.start == s.range.start)
                    }),
                high_bytes: case.file.encoding.is_single_byte()
                    && plan
                        .splices()
                        .iter()
                        .any(|s| s.bytes.iter().any(|&b| b >= 0x80)),
                rows: false,
                columns: false,
            };
            prop_assert!(done.edits_during_save.is_empty());
            prop_assert_eq!(
                done.modified,
                std::fs::metadata(&path).unwrap().modified().ok()
            );
            // The same bytes, and the same splices: F2, per field.
            check_identical(&saved.bytes, &on_disk)?;
            check_only_changed(&case.file.bytes, &on_disk, &saved.changes)?;
            let splices: Vec<Change> = plan
                .splices()
                .iter()
                .map(|s| Change::replace(s.range.clone(), s.bytes.clone()))
                .collect();
            prop_assert_eq!(&splices, &saved.changes);
            let fixes: Vec<OracleFix> = done.fixes.iter().copied().map(tk_fix).collect();
            prop_assert_eq!(&fixes, &saved.fixes);
            prop_assert_eq!(done.rows, oracle.row_count());
            prop_assert!(done.complete);
            prop_assert!(done.skipped_edits.is_empty());
            // The same encoding hint, as the file's attribute.
            let hint = done.attributes.text_encoding;
            if !chosen_encoding {
                prop_assert_eq!(hint.and_then(tk_encoding), saved.encoding_hint);
            }
            prop_assert_eq!(
                attribute(&path, TEXT_ENCODING_ATTRIBUTE_C),
                hint.map(|e| text_encoding_value(e).into_bytes())
            );
            prop_assert_eq!(
                attribute(&path, INTERPRETATION_ATTRIBUTE_C),
                done.attributes
                    .interpretation_value()
                    .map(String::into_bytes)
            );
            saved
        }
        (Err(OracleError::ReadOnly), Err(SaveError::ReadOnly), Err(SaveError::ReadOnly)) => {
            prop_assert_eq!(&std::fs::read(&opened).unwrap(), &case.file.bytes);
            return Ok(Outcome::ReadOnly);
        }
        (
            Err(OracleError::Unencodable(cells)),
            Err(SaveError::Unencodable {
                encoding: planned,
                cells: planned_cells,
            }),
            Err(SaveError::Unencodable {
                encoding: refused,
                cells: refused_cells,
            }),
        ) => {
            // F5: the same cells, before anything was written.
            prop_assert_eq!((planned, &planned_cells), (encoding, &cells));
            prop_assert_eq!((*refused, refused_cells), (encoding, &cells));
            prop_assert_eq!(&std::fs::read(&opened).unwrap(), &case.file.bytes);
            if mode == Mode::SaveAs {
                prop_assert!(!path.exists(), "nothing written");
            }
            prop_assert!(document.has_edits());
            return Ok(Outcome::Unencodable);
        }
        (expected, plan, result) => {
            prop_assert!(
                false,
                "oracle {:?}, plan {:?}, save {:?}",
                expected.map(|s| s.bytes.escape_ascii().to_string()),
                plan,
                result
            );
            unreachable!()
        }
    };

    // The rebase: the document reads the saved file, in the same lineage.
    prop_assert!(!document.has_edits());
    prop_assert_eq!(document.lineage(), lineage);
    prop_assert!(!document.original().diverged);
    // The new reading's index is built from the save's plan, complete at
    // once: the same rows as the saved bytes indexed afresh, and once its
    // index pass ends, the same field count mode.
    let reading = document.current();
    let built = RowIndex::build(&on_disk, reading.parser.dialect()).unwrap();
    prop_assert_eq!(reading.index.status(), crate::index::Status::Complete);
    prop_assert_eq!(reading.index.row_count(), built.row_count());
    for row in 0..=built.row_count() {
        prop_assert_eq!(
            reading.index.row_extent(row),
            built.row_extent(row),
            "row {}",
            row
        );
    }
    prop_assert_eq!(
        reading.index.unterminated_quote(),
        built.unterminated_quote()
    );
    // The column count is the saved file's at once, and so is every row's
    // field count: column operations needn't wait for the index pass.
    prop_assert_eq!(reading.index.field_count_mode(), built.field_count_mode());
    let columns = built.field_count_mode().unwrap_or(0);
    let screen = job.wait().ok().and_then(|done| done.reread.as_ref());
    let screen = screen.map(|screen| screen.column_count);
    prop_assert_eq!(screen, Some(columns));
    prop_assert_eq!(document.column_count(), columns);
    prop_assert!(reading.counts.is_some());
    let can = document.can_delete_column(0);
    prop_assert!(!matches!(can, Err(EditError::StillReading)), "{:?}", can);
    wait_for_index(&document);
    prop_assert_eq!(reading.index.field_count_mode(), built.field_count_mode());
    let (counts, marks) = super::saved_counts_and_marks(&reading);
    prop_assert_eq!(counts, marks);
    drop(reading);
    check_rows(&document, &oracle, &saved)?;
    check_reopen(&document, &path, &oracle, &saved, encoding)?;
    covered.rows = check_undo_after_save(&document, case, &saved, &made)?;
    covered.columns = made.iter().any(|made| made.command.is_column());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&opened);
    Ok(Outcome::Compared(covered))
}

/// The reopen property (see the module docs): `path`, saved from
/// `document`, opened afresh reads as it did, with no notes and no
/// suggestions. Leal's own single-byte attribute holds whatever the bytes
/// (ADR-0013 decision 2; `tk::reopen_encoding`); were the model to say
/// otherwise, the encoding would be chosen again, as it was at open.
fn check_reopen(
    document: &Document,
    path: &Path,
    oracle: &Oracle<'_>,
    saved: &SavedFile,
    encoding: Encoding,
) -> Result<(), TestCaseError> {
    let on_disk = std::fs::read(path).unwrap();
    let holds = tk_encoding(encoding).is_some_and(|tk_encoding| {
        tk::reopen_encoding(&on_disk, saved.encoding_hint, tk::HintWriter::Leal) == tk_encoding
            || !tk_encoding.is_single_byte()
    });
    let options = OpenOptions {
        choices: Choices {
            encoding: (!holds).then_some(encoding),
            ..Choices::default()
        },
        ..options(10)
    };
    let (reopened, _) = Document::open(
        path,
        &DIR.temp(),
        VolumeInfo::default(),
        &SCHEDULER,
        options,
        None,
    )
    .unwrap();
    let now = reopened.detection();
    let before = document.detection();
    prop_assert_eq!(now.bom, before.bom);
    prop_assert_eq!(now.encoding, before.encoding);
    prop_assert_eq!(now.delimiter, before.delimiter);
    prop_assert_eq!(now.header, before.header);
    prop_assert_eq!(&now.notes, &[]);
    wait_for_index(&reopened);
    check_rows(&reopened, oracle, saved)?;
    let review = *reopened.review_job().wait().unwrap();
    prop_assert_eq!(review.encoding_suggestion, None, "no whole-file suggestion");
    prop_assert_eq!(
        review.delimiter_suggestion,
        None,
        "no whole-file suggestion"
    );
    Ok(())
}

/// Every row of `document` reads as the oracle's, with the saved file's
/// line endings.
fn check_rows(
    document: &Document,
    oracle: &Oracle<'_>,
    saved: &SavedFile,
) -> Result<(), TestCaseError> {
    prop_assert_eq!(document.row_count(), oracle.row_count());
    let reading = document.current();
    let bytes = reading.source.as_slice().unwrap();
    for row in 0..oracle.row_count() {
        let cells = document
            .cells(row..row + 1, 0..usize::MAX, usize::MAX)
            .unwrap();
        // A row column deletes left with no cells is written `""`, so
        // it reads back as one empty field (ADR-0014 decision 6).
        let len = oracle.row_len(row).max(1);
        prop_assert_eq!(cells[0].field_count, len, "row {}", row);
        for column in 0..len {
            let value = document.full_value(row, column).unwrap();
            let expected = oracle.value(row, column).or_else(|| Some(String::new()));
            prop_assert_eq!(value, expected, "({}, {})", row, column);
        }
        let ending = reading.index.row(row, bytes).unwrap().line_ending;
        prop_assert_eq!(
            ending.map(tk_line_ending),
            saved.line_endings[row],
            "row {}",
            row
        );
    }
    Ok(())
}

/// How a Save As UTF-8 went, for the coverage test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Converted {
    /// Written, from this encoding, and compared in full.
    Written(Encoding),
    /// Refused, naming the cells the oracle names.
    Refused(Encoding),
}

/// Save As UTF-8 (ADR-0008 decision 7) of `case`, with its cell edits, to
/// a new place, against the oracle's `save_as_utf8` (see the module docs).
fn save_as_utf8_matches_the_oracle(case: &EditCase) -> Result<Converted, TestCaseError> {
    crate::document::saving::TEST_CHUNK_BYTES.store(61, Ordering::Relaxed);
    let (document, opened, _, oracle, _) = open_and_edit(case)?;
    let lineage = document.lineage();
    let before = document.detection();
    let expected = oracle.save_as_utf8();
    let path = opened.with_extension("utf8.csv");
    let job = document.save(SaveRequest::new(&path, SaveKind::SaveAsUtf8));
    let result = job.wait();
    prop_assert_eq!(&std::fs::read(&opened).unwrap(), &case.file.bytes);
    let saved = match (expected, result) {
        (Ok(saved), Ok(done)) => {
            check_identical(&saved.bytes, &std::fs::read(&path).unwrap())?;
            let fixes: Vec<OracleFix> = done.fixes.iter().copied().map(tk_fix).collect();
            prop_assert_eq!(&fixes, &saved.fixes);
            prop_assert_eq!(done.rows, oracle.row_count());
            prop_assert_eq!(done.len, u64::try_from(saved.bytes.len()).unwrap());
            prop_assert!(done.complete && done.skipped_edits.is_empty());
            // The attribute is set to UTF-8, BOM or not.
            prop_assert_eq!(done.attributes.text_encoding, Some(Encoding::Utf8));
            prop_assert_eq!(
                attribute(&path, TEXT_ENCODING_ATTRIBUTE_C),
                Some(text_encoding_value(Encoding::Utf8).into_bytes())
            );
            prop_assert_eq!(
                attribute(&path, INTERPRETATION_ATTRIBUTE_C),
                done.attributes
                    .interpretation_value()
                    .map(String::into_bytes)
            );
            saved
        }
        (
            Err(OracleError::Unconvertible(cells)),
            Err(SaveError::Unconvertible {
                encoding,
                cells: named,
                more,
            }),
        ) => {
            // F5: the same cells, and nothing written.
            prop_assert_eq!(named, &cells);
            prop_assert!(!more);
            prop_assert_eq!(*encoding, before.encoding);
            prop_assert!(!path.exists(), "nothing written");
            prop_assert_eq!(document.lineage(), lineage);
            prop_assert_eq!(document.detection().encoding, before.encoding);
            return Ok(Converted::Refused(before.encoding));
        }
        (expected, result) => {
            prop_assert!(
                false,
                "oracle {:?}, save {:?}",
                expected.map(|s| s.bytes.escape_ascii().to_string()),
                result
            );
            unreachable!()
        }
    };

    // The document reads the new file, in UTF-8, with a UTF-8 BOM if the
    // file had a BOM, and the same values; in the same lineage.
    let now = document.detection();
    prop_assert_eq!(now.encoding, Encoding::Utf8);
    let bom = if before.bom == Bom::None {
        Bom::None
    } else {
        Bom::Utf8
    };
    prop_assert_eq!(now.bom, bom);
    prop_assert_eq!(now.delimiter, before.delimiter);
    prop_assert_eq!(now.header, before.header);
    prop_assert!(!document.has_edits());
    prop_assert_eq!(document.lineage(), lineage);
    prop_assert_eq!(document.original().path, path.clone());
    // Converted, every row reads at once: the rebase waited for the index.
    if before.encoding != Encoding::Utf8 {
        let reading = document.current();
        prop_assert_eq!(reading.index.status(), crate::index::Status::Complete);
    }
    wait_for_index(&document);
    check_rows(&document, &oracle, &saved)?;
    check_reopen(&document, &path, &oracle, &saved, Encoding::Utf8)?;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&opened);
    Ok(Converted::Written(before.encoding))
}

/// Save As from an incomplete document (ADR-0008 decision 6, see the
/// module docs): the copy of `case`'s file stops at byte `at` (a drive
/// unplugged). The edits the document still takes are made on it and on
/// the oracle: cell edits on the rows it has; row and column edits wait
/// for the whole file (`StillReading`), and the cells past the cut aren't
/// there. Save As writes the oracle's rows up to the cut, byte for byte,
/// and says the save is incomplete; the document then reads the copy.
fn incomplete_save_as_matches_the_oracle(case: &EditCase, at: usize) -> Result<(), TestCaseError> {
    crate::document::saving::TEST_CHUNK_BYTES.store(61, Ordering::Relaxed);
    let (path, options, _) = write_case(case);
    let fault = Some(SimulatedFault::Disconnect { at });
    let Ok(source) = Source::open_simulating_fault(&path, &DIR.temp(), 4096, fault) else {
        return Ok(()); // the head itself couldn't be read
    };
    let Ok((document, _)) = Document::from_source(source, &SCHEDULER, options, None) else {
        return Ok(());
    };
    let document = Arc::new(document);
    let _ = document.index_job().control().wait_timeout(LONG);
    let mut oracle = case.file.document().with_existing_hint(case.existing_hint);
    for edit in case_edits(case) {
        let got = match &edit {
            OracleEdit::SetCell { row, column, value } => document.set_cell(*row, *column, value),
            OracleEdit::InsertRow { at, values } => {
                document.insert_rows(*at, std::slice::from_ref(values))
            }
            OracleEdit::DeleteRow { row } => document.delete_rows(*row, 1),
            OracleEdit::InsertColumn { at, value } => document.insert_column(*at, value),
            OracleEdit::DeleteColumn { column } => document.delete_column(*column),
        };
        match got {
            Ok(Some(_)) => prop_assert!(oracle.apply(&edit).is_ok(), "{:?}", edit),
            Ok(None) => {}
            Err(EditError::StillReading) => {
                prop_assert!(!matches!(edit, OracleEdit::SetCell { .. }), "{:?}", edit);
            }
            Err(_) => {}
        }
    }
    let rows = document.row_count();
    let complete = document.can_save();
    let out = path.with_extension("cut.csv");
    let job = document.save(SaveRequest::new(&out, SaveKind::SaveAs));
    let (saved, done) = match (oracle.save(), job.wait()) {
        (Ok(saved), Ok(done)) => (saved, done),
        (Err(OracleError::ReadOnly), Err(SaveError::ReadOnly)) => return Ok(()),
        (Err(OracleError::Unencodable(cells)), Err(SaveError::Unencodable { cells: got, .. })) => {
            prop_assert_eq!(got, &cells);
            return Ok(());
        }
        (expected, got) => {
            prop_assert!(false, "oracle {:?}, save {:?}", expected.map(|_| ()), got);
            unreachable!()
        }
    };
    let layout = saved.layout.as_ref().unwrap();
    let end = layout
        .rows
        .get(rows)
        .map_or(saved.bytes.len(), |row| row.span.start);
    check_identical(&saved.bytes[..end], &std::fs::read(&out).unwrap())?;
    prop_assert_eq!(done.rows, rows);
    prop_assert_eq!(done.complete, complete);
    prop_assert!(done.skipped_edits.is_empty());
    // The document reads the copy, every row at once, with no edits.
    prop_assert!(!document.has_edits());
    prop_assert_eq!(document.row_count(), rows);
    prop_assert!(document.can_save());
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[test]
fn incomplete_save_as_writes_the_oracles_rows_up_to_the_cut() {
    let strategy = (edit_case(larger()), 0.0..1.0_f64);
    run_scaled(64, &strategy, |(case, fraction)| {
        let len = case.file.bytes.len();
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "a place in the file"
        )]
        let at = ((len as f64) * fraction) as usize;
        incomplete_save_as_matches_the_oracle(&case, at)
    });
}

/// Larger files: past the first 64 KB and over several write chunks.
fn larger() -> CsvConfig {
    CsvConfig {
        max_rows: 3_000,
        max_fields: 8,
        max_value_chunks: 8,
        ..CsvConfig::messy()
    }
}

/// Files in every encoding Leal saves in: UTF-8 and Windows-1252 as
/// detected, and every single-byte encoding.
fn cases() -> impl Strategy<Value = EditCase> {
    prop_oneof![
        edit_case(CsvConfig::clean()),
        edit_case(CsvConfig::messy()),
        single_byte_edit_case(CsvConfig::clean()),
        single_byte_edit_case(CsvConfig::messy()),
    ]
}

/// Files in every encoding Leal reads, UTF-16 with unpaired surrogates and
/// final odd bytes included, for Save As UTF-8.
fn any_encoding_cases(config: CsvConfig) -> impl Strategy<Value = EditCase> {
    prop_oneof![
        edit_case(config),
        single_byte_edit_case(config),
        edits_for(csv_file_utf16(config), 4),
    ]
}

/// Runs `test` on a `divisor`th of `PROPTEST_CASES` (as task 2.1's
/// properties do; the `proptest!` macro would let `PROPTEST_CASES`
/// override a block's own count), with its regression seeds.
fn run_scaled<S: Strategy>(
    divisor: u32,
    strategy: &S,
    test: impl Fn(S::Value) -> Result<(), TestCaseError>,
) {
    let config = ProptestConfig::default(); // reads PROPTEST_CASES
    let mut runner = TestRunner::new(ProptestConfig {
        cases: config.cases.div_ceil(divisor),
        source_file: Some(file!()),
        ..config
    });
    if let Err(error) = runner.run(strategy, test) {
        panic!("{error}");
    }
}

/// Each case writes, saves and reopens a file: a quarter of the usual
/// number (64 by default, 5,000 under `just test-deep`).
#[test]
fn saving_matches_the_oracle_and_reopens_the_same() {
    run_scaled(4, &(cases(), modes()), |(case, mode)| {
        save_matches_the_oracle(&case, mode).map(|_| ())
    });
}

/// The same over files of up to 3,000 rows: a 64th of the usual number
/// (4 by default, 313 under `just test-deep`, 1,563 nightly), at about half
/// a second a case.
#[test]
fn saving_matches_the_oracle_on_larger_files() {
    let larger = prop_oneof![edit_case(larger()), single_byte_edit_case(larger())];
    run_scaled(64, &(larger, modes()), |(case, mode)| {
        save_matches_the_oracle(&case, mode).map(|_| ())
    });
}

/// UTF-16 files are read-only in v1: both refuse, and the file is
/// untouched.
#[test]
fn utf16_files_are_refused_as_the_oracle_refuses_them() {
    run_scaled(
        8,
        &edits_for(csv_file_utf16(CsvConfig::messy()), 4),
        |case| {
            let outcome = save_matches_the_oracle(&case, Mode::Save)?;
            prop_assert_eq!(outcome, Outcome::ReadOnly);
            Ok(())
        },
    );
}

/// Save As UTF-8 from every encoding: a quarter of the usual number.
#[test]
fn save_as_utf8_matches_the_oracle_and_reopens_the_same() {
    run_scaled(
        4,
        &prop_oneof![
            any_encoding_cases(CsvConfig::clean()),
            any_encoding_cases(CsvConfig::messy()),
        ],
        |case| save_as_utf8_matches_the_oracle(&case).map(|_| ()),
    );
}

/// The same over files of up to 3,000 rows, past the first 64 KB and over
/// many write chunks: a 64th of the usual number.
#[test]
fn save_as_utf8_matches_the_oracle_on_larger_files() {
    run_scaled(64, &any_encoding_cases(larger()), |case| {
        save_as_utf8_matches_the_oracle(&case).map(|_| ())
    });
}

/// The properties compare enough of every kind of case: with a fixed seed,
/// most saves are compared in full, enough of them with hatched cells,
/// fixes, files that quote every field and characters other than ASCII in
/// a single-byte encoding; enough are refused for a character the
/// encoding can't represent; and Save As UTF-8 is written from every
/// single-byte encoding and UTF-16, and refused, from both.
#[test]
fn the_save_properties_compare_most_cases() {
    let mut runner = TestRunner::new_with_rng(
        ProptestConfig::with_cases(400),
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    let strategy = (cases(), modes());
    let (mut compared, mut unencodable) = (0, 0);
    let (mut hatched, mut fixed, mut quote_all, mut high_bytes) = (0, 0, 0, 0);
    let (mut rows, mut columns) = (0, 0);
    for _ in 0..400 {
        let (case, mode) = strategy.new_tree(&mut runner).unwrap().current();
        match save_matches_the_oracle(&case, mode).unwrap() {
            Outcome::Compared(covered) => {
                compared += 1;
                hatched += usize::from(covered.hatched);
                fixed += usize::from(covered.fixed);
                quote_all += usize::from(covered.quote_all);
                high_bytes += usize::from(covered.high_bytes);
                rows += usize::from(covered.rows);
                columns += usize::from(covered.columns);
            }
            Outcome::Unencodable => unencodable += 1,
            Outcome::ReadOnly => {}
        }
    }
    let converting = any_encoding_cases(CsvConfig::messy());
    let mut written = std::collections::HashSet::new();
    let (mut refused_single, mut refused_utf16) = (0, 0);
    for _ in 0..800 {
        let case = converting.new_tree(&mut runner).unwrap().current();
        match save_as_utf8_matches_the_oracle(&case).unwrap() {
            Converted::Written(encoding) => {
                written.insert(encoding);
            }
            Converted::Refused(Encoding::Utf16Le | Encoding::Utf16Be) => refused_utf16 += 1,
            Converted::Refused(_) => refused_single += 1,
        }
    }
    eprintln!(
        "saves: {compared} compared ({hatched} hatched, {fixed} fixed, {quote_all} quoting every field, {high_bytes} writing high bytes, {rows} with rows changed, {columns} with columns changed), {unencodable} unencodable; Save As UTF-8 written from {} encodings, refused {refused_single} single-byte, {refused_utf16} UTF-16",
        written.len()
    );
    assert!(
        compared >= 320,
        "{compared} compared, {unencodable} refused"
    );
    assert!(hatched >= MIN_HATCHED, "{hatched} with a hatched cell");
    assert!(fixed >= MIN_FIXED, "{fixed} with a fix");
    assert!(
        quote_all >= MIN_QUOTE_ALL,
        "{quote_all} quoting every field"
    );
    assert!(
        high_bytes >= MIN_HIGH_BYTES,
        "{high_bytes} writing high bytes"
    );
    assert!(unencodable >= MIN_UNENCODABLE, "{unencodable} unencodable");
    assert!(rows >= MIN_ROWS, "{rows} with rows changed");
    assert!(columns >= MIN_COLUMNS, "{columns} with columns changed");
    assert!(written.contains(&Encoding::Utf16Le) || written.contains(&Encoding::Utf16Be));
    assert!(written.len() >= 10, "written from {written:?}");
    assert!(
        refused_single >= MIN_REFUSED_SINGLE_BYTE,
        "{refused_single} single-byte refused"
    );
    assert!(
        refused_utf16 >= MIN_REFUSED_UTF16,
        "{refused_utf16} UTF-16 refused"
    );
}

/// The coverage floors, about half of what the fixed seed gives (46, 47,
/// 21, 72, 35, 5 and 62 when written, task 2.3). Few single-byte
/// encodings leave bytes unassigned, so few of their Save As UTF-8 are
/// refused here; the testkit's `encoding_strategies_reach_every_case`
/// covers that case more. Task 2.4b's quoted text columns changed what the
/// seed generates, leaving one such refusal in 400 cases, so the Save As
/// UTF-8 half runs 800 (5 refused, 121 from UTF-16).
const MIN_HATCHED: usize = 23;
const MIN_FIXED: usize = 23;
const MIN_QUOTE_ALL: usize = 10;
const MIN_HIGH_BYTES: usize = 36;
const MIN_UNENCODABLE: usize = 17;
/// About half of what the fixed seed gives (115 when written, task 2.4c's
/// rows; 110 rows and 111 columns once column edits and undos before the
/// save came in).
const MIN_ROWS: usize = 57;
const MIN_COLUMNS: usize = 55;
const MIN_REFUSED_SINGLE_BYTE: usize = 2;
const MIN_REFUSED_UTF16: usize = 31;
