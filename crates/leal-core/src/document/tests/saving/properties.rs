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
//! The case's existing encoding hint is passed through as a real
//! `com.apple.TextEncoding` on the file before it is opened. The edit
//! strategy also makes row and column inserts and deletes, which are task
//! 2.4's; only its cell edits are replayed (as in task 2.1), with the oracle
//! replaying the same ones, so some name a row that isn't there and both
//! refuse them.

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
use crate::source::{INTERPRETATION_ATTRIBUTE_C, TEXT_ENCODING_ATTRIBUTE_C};

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

fn set(row: usize, column: usize, value: &str) -> OracleEdit {
    OracleEdit::SetCell {
        row,
        column,
        value: value.to_owned(),
    }
}

/// The case's cell edits in the order the strategy tried them, the ones
/// the oracle refused (ADR-0004 decision 8) included at their places.
fn cell_edits(case: &EditCase) -> Vec<(usize, usize, String)> {
    let cell = |edit: &OracleEdit| match edit {
        OracleEdit::SetCell { row, column, value } => Some((*row, *column, value.clone())),
        _ => None,
    };
    let mut all = Vec::new();
    let mut refused = case.refused.iter().peekable();
    for (at, edit) in case.edits.iter().enumerate() {
        while let Some((_, edit)) = refused.next_if(|(place, _)| *place == at) {
            all.extend(cell(edit));
        }
        all.extend(cell(edit));
    }
    all.extend(refused.filter_map(|(_, edit)| cell(edit)));
    all
}

/// Opens `case`'s file ([`open_case`]) and replays its cell edits on the
/// document and on the oracle, which must accept and refuse the same.
fn open_and_edit(
    case: &EditCase,
) -> Result<(Arc<Document>, PathBuf, bool, Oracle<'_>), TestCaseError> {
    let (document, opened, chosen_encoding) = open_case(case);
    let mut oracle = case.file.document().with_existing_hint(case.existing_hint);
    for (row, column, value) in cell_edits(case) {
        let expected = oracle.apply(&set(row, column, &value));
        let got = document.set_cell(row, column, &value);
        match (&expected, &got) {
            (Ok(()), Ok(_))
            | (Err(OracleError::InvalidEdit(_)), Err(EditError::NoSuchRow { .. }))
            | (
                Err(OracleError::AfterUnterminatedQuote(_)),
                Err(EditError::AfterUnterminatedQuote { .. }),
            ) => {}
            _ => prop_assert!(false, "oracle {:?}, document {:?}", expected, got),
        }
    }
    Ok((document, opened, chosen_encoding, oracle))
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
    let (document, opened, chosen_encoding, oracle) = open_and_edit(case)?;
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

    let covered;
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
    // The new reading's first rows come from the old index shifted by the
    // splices: the same rows as the saved bytes indexed afresh.
    {
        let reading = document.current();
        let built = RowIndex::build(&on_disk, reading.parser.dialect()).unwrap();
        prop_assert_eq!(reading.head_rows, built.row_count());
        for row in 0..reading.head_rows {
            prop_assert_eq!(
                reading.head_index.row_extent(row),
                built.row_extent(row),
                "row {}",
                row
            );
        }
        prop_assert_eq!(
            reading.head_index.unterminated_quote(),
            built.unterminated_quote()
        );
    }
    wait_for_index(&document);
    check_rows(&document, &oracle, &saved)?;
    check_reopen(&document, &path, &oracle, &saved, encoding)?;
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
        prop_assert_eq!(cells[0].field_count, oracle.row_len(row), "row {}", row);
        for column in 0..oracle.row_len(row) {
            let value = document.full_value(row, column).unwrap();
            prop_assert_eq!(value, oracle.value(row, column), "({}, {})", row, column);
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
    let (document, opened, _, oracle) = open_and_edit(case)?;
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
    for _ in 0..400 {
        let (case, mode) = strategy.new_tree(&mut runner).unwrap().current();
        match save_matches_the_oracle(&case, mode).unwrap() {
            Outcome::Compared(covered) => {
                compared += 1;
                hatched += usize::from(covered.hatched);
                fixed += usize::from(covered.fixed);
                quote_all += usize::from(covered.quote_all);
                high_bytes += usize::from(covered.high_bytes);
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
        "saves: {compared} compared ({hatched} hatched, {fixed} fixed, {quote_all} quoting every field, {high_bytes} writing high bytes), {unencodable} unencodable; Save As UTF-8 written from {} encodings, refused {refused_single} single-byte, {refused_utf16} UTF-16",
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
const MIN_REFUSED_SINGLE_BYTE: usize = 2;
const MIN_REFUSED_UTF16: usize = 31;
