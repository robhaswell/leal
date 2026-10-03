//! Self-tests for the save oracle, the edit strategy and the UTF-16
//! generator, checked against the reference parser.

mod oracle;

use leal_testkit::diagnostics::{self, DiagnosticKind};
use leal_testkit::dialect::{
    Bom, Delimiter, Encoding, UTF8_BOM, UTF16BE_BOM, UTF16LE_BOM, decode_value, encode_value,
    expected_encoding, reopen_encoding, unassigned_bytes,
};
use leal_testkit::fidelity::{Change, apply_changes, check_identical};
use leal_testkit::layout::Layout;
use leal_testkit::save::{CellSource, Document, Edit, Fix, SaveError};
use leal_testkit::strategies::csv::{CsvConfig, csv_file, csv_file_utf16};
use leal_testkit::strategies::edits::{EditCase, edit_case, edits_for, single_byte_edit_case};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::Index;

/// Replays a case's edits on a fresh document and returns each row's
/// display values.
fn final_values(case: &EditCase) -> Vec<Vec<String>> {
    let mut doc = case.file.document();
    for e in &case.edits {
        doc.apply(e).expect("strategy edits are valid");
    }
    (0..doc.row_count())
        .map(|r| {
            (0..doc.row_len(r))
                .map(|c| doc.value(r, c).unwrap_or_default())
                .collect()
        })
        .collect()
}

proptest! {
    /// The strongest check on the save oracle: after any edits to a clean
    /// file, the expected bytes parse back to exactly the document's values.
    /// This tests the quoting rule against the parser, not against itself.
    #[test]
    fn saved_bytes_parse_back_to_the_edited_values(case in edit_case(CsvConfig::clean())) {
        let saved = case.saved.clone().map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert_eq!(&saved.bytes, &apply_changes(&case.file.bytes, &saved.changes));
        let values = final_values(&case);
        // ADR-0004 decision 6: a row left with no cells is written as `""`,
        // so it reads back as one empty field.
        let values: Vec<Vec<String>> = values
            .into_iter()
            .map(|r| if r.is_empty() { vec![String::new()] } else { r })
            .collect();
        // ADR-0004 decision 7: edits never create (or remove) a BOM.
        prop_assert_eq!(Bom::detect(&saved.bytes), Bom::detect(&case.file.bytes));

        let parsed = oracle::analyze(&saved.bytes, case.file.delimiter(), case.file.encoding);
        prop_assert_eq!(parsed.check_tiles(case.file.delimiter()), Ok(()));
        let got: Vec<Vec<String>> = parsed
            .layout
            .rows
            .iter()
            .map(|r| r.fields.iter().map(|f| decode_value(&f.value, case.file.encoding)).collect())
            .collect();
        prop_assert_eq!(got, values);
        // Clean files stay clean: no text after quotes, no unterminated quote.
        let clean = parsed
            .layout
            .rows
            .iter()
            .flat_map(|r| &r.fields)
            .all(|f| f.text_after_quote.is_none() && !f.unterminated);
        prop_assert!(clean, "an edit made the file messy");
    }

    /// Messy files: saving succeeds, except for values Windows-1252 can't
    /// encode (F5); the splices always describe the output exactly.
    #[test]
    fn messy_edits_save_or_name_the_unencodable_cells(case in edit_case(CsvConfig::messy())) {
        match &case.saved {
            Ok(saved) => {
                prop_assert_eq!(&saved.bytes, &apply_changes(&case.file.bytes, &saved.changes));
                if case.edits.is_empty() {
                    check_identical(&case.file.bytes, &saved.bytes)?;
                }
            }
            Err(SaveError::Unencodable(cells)) => {
                prop_assert_eq!(case.file.encoding, Encoding::Windows1252);
                prop_assert!(!cells.is_empty());
                let values = final_values(&case);
                for &(r, c) in cells {
                    let unencodable = encode_value(&values[r][c], Encoding::Windows1252).is_err();
                    prop_assert!(unencodable, "{:?}", values[r][c]);
                }
            }
            Err(e) => prop_assert!(false, "unexpected {}", e),
        }
    }

    /// F3: setting cells and then setting them all back restores the
    /// original bytes exactly, on any file.
    #[test]
    fn setting_cells_back_restores_the_original(
        file in csv_file(CsvConfig::messy()),
        cells in vec((any::<Index>(), any::<Index>(), any::<Index>()), 1..6),
    ) {
        if file.layout.rows.is_empty() {
            return Ok(());
        }
        let mut doc = file.document();
        let mut touched = Vec::new();
        for (r, c, v) in cells {
            let row = r.index(doc.row_count());
            let column = c.index(doc.row_len(row));
            let value = v.get(&leal_testkit::strategies::edits::EDIT_VALUES).to_string();
            doc.apply(&Edit::SetCell { row, column, value }).unwrap();
            touched.push((row, column));
        }
        for (row, column) in touched {
            let value = doc.original_value(row, column).unwrap();
            doc.apply(&Edit::SetCell { row, column, value }).unwrap();
        }
        let saved = doc.save().map_err(|e| TestCaseError::fail(e.to_string()))?;
        prop_assert!(saved.changes.is_empty());
        check_identical(&file.bytes, &saved.bytes)?;
    }

    /// The UTF-16 generator's spans are file byte offsets, and its
    /// diagnostics (from the shared `derive`) match the oracle's own UTF-16
    /// NUL and surrogate scan.
    #[test]
    fn utf16_files_round_trip(file in csv_file_utf16(CsvConfig::messy())) {
        prop_assert!(matches!(file.encoding, Encoding::Utf16Le | Encoding::Utf16Be));
        let a = oracle::analyze(&file.bytes, file.delimiter(), file.encoding);
        prop_assert_eq!(a.check_tiles(file.delimiter()), Ok(()));
        prop_assert_eq!(&a.layout, &file.layout);
        prop_assert_eq!(&a.diagnostics, &file.diagnostics);
    }

    /// UTF-16 is read-only in v1, so any save of it fails.
    #[test]
    fn utf16_saves_are_refused(case in edits_for(csv_file_utf16(CsvConfig::clean()), 2)) {
        prop_assert_eq!(case.saved.unwrap_err(), SaveError::ReadOnly);
    }

    /// Every single-byte encoding (task 2.3): a save's splices describe its
    /// output, which reads back in that encoding as the document's values;
    /// or it names exactly the cells holding a character the encoding
    /// can't write (F5).
    #[test]
    fn single_byte_edits_save_or_name_the_unencodable_cells(
        case in single_byte_edit_case(CsvConfig::clean()),
    ) {
        let encoding = case.file.encoding;
        let values = final_values(&case);
        let unencodable: Vec<(usize, usize)> = cells_where(&case, |r, c| {
            edited(&case, r, c) && encode_value(&values[r][c], encoding).is_err()
        });
        match &case.saved {
            Ok(saved) => {
                prop_assert!(unencodable.is_empty());
                prop_assert_eq!(&saved.bytes, &apply_changes(&case.file.bytes, &saved.changes));
                let parsed = oracle::analyze(&saved.bytes, case.file.delimiter(), encoding);
                prop_assert_eq!(read_back(&parsed.layout, encoding), padded(values));
            }
            Err(SaveError::Unencodable(cells)) => prop_assert_eq!(cells, &unencodable),
            Err(e) => prop_assert!(false, "unexpected {}", e),
        }
    }

    /// Save As UTF-8 (ADR-0008 decision 7) from clean files in any
    /// encoding: the output is UTF-8, with a UTF-8 BOM exactly when the
    /// file had a BOM, and reads back as the document's values; or it
    /// names exactly the unedited cells whose bytes aren't text in the
    /// file's encoding (F5).
    #[test]
    fn save_as_utf8_reads_back_the_same_values(case in any_encoding_case(CsvConfig::clean())) {
        let mut doc = case.file.document();
        for e in &case.edits {
            doc.apply(e).expect("strategy edits are valid");
        }
        let expected = unconvertible_cells(&case, &doc);
        match doc.save_as_utf8() {
            Ok(saved) => {
                prop_assert_eq!(&expected, &Vec::new());
                prop_assert_eq!(saved.encoding_hint, Some(Encoding::Utf8));
                let had_bom = Bom::detect(&case.file.bytes) != Bom::None;
                let bom = if had_bom { Bom::Utf8 } else { Bom::None };
                prop_assert_eq!(Bom::detect(&saved.bytes), bom);
                if case.file.encoding == Encoding::Utf8 {
                    prop_assert_eq!(Ok(saved.bytes), doc.save().map(|s| s.bytes));
                } else {
                    prop_assert!(std::str::from_utf8(&saved.bytes).is_ok());
                    let parsed = oracle::analyze(&saved.bytes, case.file.delimiter(), Encoding::Utf8);
                    prop_assert_eq!(parsed.check_tiles(case.file.delimiter()), Ok(()));
                    prop_assert_eq!(
                        read_back(&parsed.layout, Encoding::Utf8),
                        padded(final_values(&case))
                    );
                    let endings: Vec<_> = parsed.layout.rows.iter().map(|r| r.line_ending).collect();
                    prop_assert_eq!(endings, saved.line_endings);
                }
            }
            Err(SaveError::Unconvertible(cells)) => {
                prop_assert!(!cells.is_empty());
                prop_assert_eq!(cells, expected);
            }
            Err(e) => prop_assert!(false, "unexpected {}", e),
        }
    }

    /// The same refusals from messy files, whose unpaired surrogates, final
    /// odd bytes and unassigned bytes are where the cells come from.
    #[test]
    fn save_as_utf8_names_exactly_the_cells_it_cant_convert(
        case in any_encoding_case(CsvConfig::messy()),
    ) {
        let mut doc = case.file.document();
        for e in &case.edits {
            doc.apply(e).expect("strategy edits are valid");
        }
        let expected = unconvertible_cells(&case, &doc);
        match doc.save_as_utf8() {
            Ok(saved) => {
                prop_assert_eq!(&expected, &Vec::new());
                if case.file.encoding != Encoding::Utf8 {
                    prop_assert!(std::str::from_utf8(&saved.bytes).is_ok());
                }
            }
            Err(SaveError::Unconvertible(cells)) => prop_assert_eq!(cells, expected),
            Err(e) => prop_assert!(false, "unexpected {}", e),
        }
    }
}

/// Edit cases in every encoding: UTF-8 and Windows-1252 as detected, the
/// other single-byte encodings, and UTF-16.
fn any_encoding_case(config: CsvConfig) -> impl Strategy<Value = EditCase> {
    prop_oneof![
        edit_case(config),
        single_byte_edit_case(config),
        edits_for(csv_file_utf16(config), 4),
    ]
}

/// The cells of the edited document (row, column) where `pick` holds.
fn cells_where(case: &EditCase, pick: impl Fn(usize, usize) -> bool) -> Vec<(usize, usize)> {
    let values = final_values(case);
    (0..values.len())
        .flat_map(|r| (0..values[r].len()).map(move |c| (r, c)))
        .filter(|&(r, c)| pick(r, c))
        .collect()
}

/// Whether the edited document's cell holds a value of its own.
fn edited(case: &EditCase, row: usize, column: usize) -> bool {
    let mut doc = case.file.document();
    for e in &case.edits {
        doc.apply(e).expect("strategy edits are valid");
    }
    doc.cell_source(row, column) == Some(CellSource::Edited)
}

/// Each row's values as `layout` reads them in `encoding`.
fn read_back(layout: &Layout, encoding: Encoding) -> Vec<Vec<String>> {
    layout
        .rows
        .iter()
        .map(|r| {
            r.fields
                .iter()
                .map(|f| decode_value(&f.value, encoding))
                .collect()
        })
        .collect()
}

/// ADR-0004 decision 6: a row left with no cells is written as `""`, so it
/// reads back as one empty field.
fn padded(values: Vec<Vec<String>>) -> Vec<Vec<String>> {
    values
        .into_iter()
        .map(|r| if r.is_empty() { vec![String::new()] } else { r })
        .collect()
}

/// The cells Save As UTF-8 must name: those of `doc` (the case's file with
/// its edits) that still hold an original field with bytes that aren't
/// text in the file's encoding. Found from the bytes, not by converting.
fn unconvertible_cells(case: &EditCase, doc: &Document<'_>) -> Vec<(usize, usize)> {
    let file = &case.file;
    let offsets: Vec<usize> = match file.encoding {
        // Not converted: its bytes are kept as they are.
        Encoding::Utf8 => Vec::new(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            diagnostics::utf16_nul_and_invalid_offsets(
                &file.bytes,
                file.layout.bom_len,
                file.encoding == Encoding::Utf16Le,
            )
            .1
        }
        single_byte => {
            let unassigned = unassigned_bytes(single_byte);
            (0..file.bytes.len())
                .filter(|&at| unassigned[usize::from(file.bytes[at])])
                .collect()
        }
    };
    let bad: Vec<(usize, usize)> = offsets
        .iter()
        .filter_map(|&at| file.layout.field_of_offset(at))
        .collect();
    let mut cells = Vec::new();
    for r in 0..doc.row_count() {
        for c in 0..doc.row_len(r) {
            if let (Some(CellSource::Original { field }), Some(source)) =
                (doc.cell_source(r, c), doc.source_row(r))
                && bad.contains(&(source, field))
            {
                cells.push((r, c));
            }
        }
    }
    cells
}

#[test]
fn edit_strategy_reaches_every_kind_of_edit() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = edit_case(CsvConfig::messy());
    let cases: Vec<EditCase> = (0..1000)
        .map(|_| strategy.new_tree(&mut runner).unwrap().current())
        .collect();
    let edits = || cases.iter().flat_map(|c| &c.edits);
    let has = |pred: &dyn Fn(&Edit) -> bool| edits().any(pred);
    assert!(has(&|e| matches!(e, Edit::SetCell { .. })));
    assert!(has(&|e| matches!(e, Edit::InsertRow { .. })));
    assert!(has(&|e| matches!(e, Edit::DeleteRow { .. })));
    assert!(has(&|e| matches!(e, Edit::InsertColumn { .. })));
    assert!(has(&|e| matches!(e, Edit::DeleteColumn { .. })));
    assert!(has(
        &|e| matches!(e, Edit::SetCell { value, .. } if value.contains('"'))
    ));
    // Some cases edit and then revert, leaving the file byte-identical.
    assert!(cases.iter().any(|c| !c.edits.is_empty()
        && c.saved.as_ref().is_ok_and(|s| s.bytes == c.file.bytes)));
    // Some cases fail F5 (an emoji in a Windows-1252 file).
    assert!(
        cases
            .iter()
            .any(|c| matches!(c.saved, Err(SaveError::Unencodable(_))))
    );
    // Some saves need an encoding hint (ADR-0004 decision 11), so the reopen
    // property really tests it: some write none, and some write one only
    // because the guess would differ (the file had no hint before).
    assert!(
        cases
            .iter()
            .any(|c| c.saved.as_ref().is_ok_and(|s| s.encoding_hint.is_some()))
    );
    assert!(
        cases
            .iter()
            .any(|c| c.saved.as_ref().is_ok_and(|s| s.encoding_hint.is_none()))
    );
    assert!(
        cases.iter().any(|c| c.existing_hint.is_none()
            && c.saved.as_ref().is_ok_and(|s| s.encoding_hint.is_some()))
    );
    // Inserted rows are as wide as the file's rows, not always one field.
    assert!(has(
        &|e| matches!(e, Edit::InsertRow { values, .. } if values.len() > 1)
    ));
    // Some saves change exactly one field.
    assert!(
        cases
            .iter()
            .any(|c| c.saved.as_ref().is_ok_and(|s| s.changes.len() == 1
                && c.edits.len() == 1
                && matches!(c.edits[0], Edit::SetCell { .. })))
    );
}

/// The task 2.3 strategies reach what they are for: every single-byte
/// encoding, saves in each of them that write characters other than ASCII
/// and that are refused (F5), and Save As UTF-8 refusals from an unpaired
/// surrogate, a final odd byte and an unassigned single byte.
#[test]
fn encoding_strategies_reach_every_case() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let single = single_byte_edit_case(CsvConfig::messy());
    let mut encodings = std::collections::HashSet::new();
    let (mut written, mut refused) = (0, 0);
    for _ in 0..2000 {
        let case = single.new_tree(&mut runner).unwrap().current();
        encodings.insert(case.file.encoding);
        match &case.saved {
            Ok(saved)
                if saved
                    .changes
                    .iter()
                    .any(|c| c.replacement.iter().any(|&b| b >= 0x80)) =>
            {
                written += 1;
            }
            Err(SaveError::Unencodable(_)) => refused += 1,
            _ => {}
        }
    }
    assert_eq!(encodings.len(), Encoding::SINGLE_BYTE.len());
    assert!(written >= 100, "{written} saves writing high bytes");
    assert!(refused >= 100, "{refused} refused");

    let utf16 = edits_for(csv_file_utf16(CsvConfig::messy()), 4);
    let (mut surrogate, mut odd) = (0, 0);
    for _ in 0..2000 {
        let case = utf16.new_tree(&mut runner).unwrap().current();
        let mut doc = case.file.document();
        for e in &case.edits {
            doc.apply(e).unwrap();
        }
        if let Err(SaveError::Unconvertible(_)) = doc.save_as_utf8() {
            let (_, invalid) = diagnostics::utf16_nul_and_invalid_offsets(
                &case.file.bytes,
                2,
                case.file.encoding == Encoding::Utf16Le,
            );
            if invalid.iter().any(|&at| at + 1 == case.file.bytes.len())
                && case.file.bytes.len() % 2 == 1
            {
                odd += 1;
            } else {
                surrogate += 1;
            }
        }
    }
    assert!(
        surrogate >= 50,
        "{surrogate} refused for an unpaired surrogate"
    );
    assert!(odd >= 20, "{odd} refused for a final odd byte");

    let unassigned = (0..2000)
        .map(|_| single.new_tree(&mut runner).unwrap().current())
        .filter(|case| {
            let mut doc = case.file.document();
            for e in &case.edits {
                doc.apply(e).unwrap();
            }
            matches!(doc.save_as_utf8(), Err(SaveError::Unconvertible(_)))
        })
        .count();
    eprintln!(
        "single-byte saves: {written} writing high bytes, {refused} refused; Save As UTF-8 refused: {surrogate} for a surrogate, {odd} for an odd byte, {unassigned} for an unassigned byte"
    );
    assert!(
        unassigned >= 20,
        "{unassigned} refused for an unassigned byte"
    );
}

#[test]
fn parse_back_property_checks_most_cases() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;

    let mut runner = TestRunner::deterministic();
    let strategy = edit_case(CsvConfig::clean());
    let checked = (0..500)
        .map(|_| strategy.new_tree(&mut runner).unwrap().current())
        .filter(|c| !c.edits.is_empty() && c.saved.is_ok())
        .count();
    // Nothing is skipped any more; this guards against the strategy
    // producing mostly empty edit lists.
    assert!(
        checked >= 350,
        "only {checked} of 500 cases have edits to check"
    );
}

/// A corpus case, parsed by the oracle.
fn corpus_case(name: &str) -> (Vec<u8>, Layout, Delimiter, Encoding) {
    let case = leal_testkit::corpus::load()
        .unwrap()
        .into_iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no corpus case {name}"));
    let d = case.sidecar.dialect.delimiter;
    let enc = case.sidecar.dialect.encoding;
    let layout = oracle::analyze(&case.bytes, d, enc).layout;
    (case.bytes, layout, d, enc)
}

fn insert_row(at: usize) -> Edit {
    Edit::InsertRow {
        at,
        values: vec!["x".into(), "y".into()],
    }
}

/// ADR-0004 decision 8, on `id,note\n1,ok\n2,"never closed\n3,lost\n`.
#[test]
fn nothing_can_be_inserted_inside_an_unterminated_quote() {
    let (bytes, layout, d, enc) = corpus_case("diagnostics/unterminated-quote.csv");
    let mut doc = Document::new(&bytes, &layout, d, enc);
    assert_eq!(doc.unterminated(), Some((2, 1)));

    // After the quote's row, or after its field: refused, nothing changes.
    assert_eq!(
        doc.apply(&insert_row(3)),
        Err(SaveError::AfterUnterminatedQuote(insert_row(3)))
    );
    let col = Edit::InsertColumn {
        at: 2,
        value: "z".into(),
    };
    assert_eq!(
        doc.apply(&col),
        Err(SaveError::AfterUnterminatedQuote(col.clone()))
    );
    // A hatched cell past it, too (ADR-0005 decision 2), but not one of
    // the shorter row before it.
    let hatched = Edit::SetCell {
        row: 2,
        column: 2,
        value: "z".into(),
    };
    assert_eq!(
        doc.apply(&hatched),
        Err(SaveError::AfterUnterminatedQuote(hatched.clone()))
    );
    assert_eq!(doc.save().unwrap().bytes, bytes);

    // Before it is fine: the new bytes land ahead of the opening quote.
    doc.apply(&insert_row(2)).unwrap();
    let col = Edit::InsertColumn {
        at: 1,
        value: "z".into(),
    };
    doc.apply(&col).unwrap();
    assert_eq!(doc.unterminated(), Some((3, 2)));
    assert_eq!(
        doc.save().unwrap().bytes,
        b"id,z,note\n1,z,ok\nx,z,y\n2,z,\"never closed\n3,lost\n"
    );

    // Editing the swallowed cell writes a closing quote, so rows can follow.
    let fix = Edit::SetCell {
        row: 3,
        column: 2,
        value: "fixed".into(),
    };
    doc.apply(&fix).unwrap();
    assert_eq!(doc.unterminated(), None);
    doc.apply(&insert_row(4)).unwrap();
    assert_eq!(
        doc.save().unwrap().bytes,
        b"id,z,note\n1,z,ok\nx,z,y\n2,z,\"fixed\"\nx,y"
    );

    // Setting it back to its original value would reopen the quote over the
    // row that now follows it, so that is refused too (found by proptest).
    let revert = Edit::SetCell {
        row: 3,
        column: 2,
        value: "never closed\n3,lost\n".into(),
    };
    assert_eq!(
        doc.apply(&revert),
        Err(SaveError::AfterUnterminatedQuote(revert.clone()))
    );
    // Once the row after it is gone, the revert is allowed again.
    doc.apply(&Edit::DeleteRow { row: 4 }).unwrap();
    doc.apply(&revert).unwrap();
    assert_eq!(
        doc.save().unwrap().bytes,
        b"id,z,note\n1,z,ok\nx,z,y\n2,z,\"never closed\n3,lost\n"
    );
}

proptest! {
    /// ADR-0004 decision 8: generated edits never insert after an
    /// unterminated quote (the oracle refuses them and the strategy drops
    /// them), so the quote, if still there, is still the end of the file.
    #[test]
    fn edits_never_land_inside_an_unterminated_quote(case in edit_case(CsvConfig::messy())) {
        let mut doc = case.file.document();
        for e in &case.edits {
            prop_assert!(doc.apply(e).is_ok(), "{:?} was generated but is refused", e);
        }
        if let (Some(_), Ok(saved)) = (doc.unterminated(), &case.saved) {
            let original = case.file.layout.rows.last().and_then(|r| r.fields.last());
            let parsed = oracle::analyze(&saved.bytes, case.file.delimiter(), case.file.encoding);
            let last = parsed.layout.rows.last().and_then(|r| r.fields.last());
            prop_assert!(last.is_some_and(|f| f.unterminated));
            // It swallows exactly the bytes it swallowed before: nothing was
            // added inside it. (Row structure elsewhere is checked by
            // `reopening_a_saved_file_gives_the_same_structure`.)
            let raw = |b: &[u8], f: Option<&leal_testkit::layout::FieldLayout>| {
                f.map(|f| b[f.span.clone()].to_vec())
            };
            prop_assert_eq!(raw(&saved.bytes, last), raw(&case.file.bytes, original));
        }
    }
}

/// ADR-0004 decision 9, on `diagnostics/invalid-utf8.csv`: setting a cell
/// with invalid bytes to its displayed value is no edit, so the invalid
/// bytes come back. A different value replaces them.
#[test]
fn setting_the_displayed_value_keeps_invalid_bytes() {
    let (bytes, layout, d, enc) = corpus_case("diagnostics/invalid-utf8.csv");
    let mut doc = Document::new(&bytes, &layout, d, enc);
    assert_eq!(doc.value(2, 0).as_deref(), Some("Ren\u{FFFD}"));
    let same = Edit::SetCell {
        row: 2,
        column: 0,
        value: "Ren\u{FFFD}".into(),
    };
    doc.apply(&same).unwrap();
    let saved = doc.save().unwrap();
    assert!(saved.changes.is_empty());
    check_identical(&bytes, &saved.bytes).unwrap();

    let different = Edit::SetCell {
        row: 2,
        column: 0,
        value: "René".into(),
    };
    doc.apply(&different).unwrap();
    let span = layout.rows[2].fields[0].span.clone();
    assert_eq!(
        doc.save().unwrap().changes,
        vec![Change::replace(span, "René")]
    );
}

proptest! {
    /// ADR-0004 decision 10, the reopen invariant, on every generated edit
    /// case (clean and messy): opening the saved file gives the same
    /// encoding, BOM and rows (count, line endings and every value) as the
    /// document had. The delimiter is the file's own; the testkit has no
    /// delimiter or header detector, so those are left to task 1.2.
    #[test]
    fn reopening_a_saved_file_gives_the_same_structure(
        case in prop_oneof![edit_case(CsvConfig::clean()), edit_case(CsvConfig::messy())]
    ) {
        let Ok(saved) = &case.saved else {
            return Ok(()); // F5 failures and read-only files have no output
        };
        let values: Vec<Vec<String>> = final_values(&case)
            .into_iter()
            .map(|r| if r.is_empty() { vec![String::new()] } else { r })
            .collect();

        prop_assert_eq!(Bom::detect(&saved.bytes), Bom::detect(&case.file.bytes));
        // ADR-0004 decision 11: reopening *with* the saved encoding hint gives
        // exactly the document's encoding. No flip is allowed. The hint is
        // written when it is needed, or to update one the file already had.
        let guess = expected_encoding(&saved.bytes);
        let needed = guess != case.file.encoding || case.existing_hint.is_some();
        prop_assert_eq!(saved.encoding_hint, needed.then_some(case.file.encoding));
        let reopened = reopen_encoding(&saved.bytes, saved.encoding_hint);
        prop_assert_eq!(reopened, case.file.encoding);

        let parsed = oracle::analyze(&saved.bytes, case.file.delimiter(), case.file.encoding);
        let endings: Vec<_> = parsed.layout.rows.iter().map(|r| r.line_ending).collect();
        prop_assert_eq!(&endings, &saved.line_endings);
        let got: Vec<Vec<String>> = parsed
            .layout
            .rows
            .iter()
            .map(|r| r.fields.iter().map(|f| decode_value(&f.value, case.file.encoding)).collect())
            .collect();
        prop_assert_eq!(got, values);

        // A UTF-8 hint is honoured even over invalid bytes; those get the
        // usual invalid-encoding warning (ADR-0004 decision 11).
        if reopened == Encoding::Utf8 && std::str::from_utf8(&saved.bytes).is_err() {
            let diagnostics = diagnostics::derive(&parsed.layout, &saved.bytes, reopened);
            prop_assert!(diagnostics.iter().any(|d| d.kind == DiagnosticKind::InvalidEncoding));
        }
    }
}

/// What happens *without* the encoding hint, for example in another app
/// that doesn't read `com.apple.TextEncoding`, or after the file travels by
/// email or git: the three ways an edit can flip the guessed encoding of a
/// BOM-less file (found by the reopen property). Each edit clears one cell.
/// With the hint (ADR-0004 decision 11), each reopens in its own encoding.
#[test]
fn encoding_can_flip_on_reopen() {
    let flip = |bytes: &[u8], row: usize| {
        let enc = expected_encoding(bytes);
        let layout = oracle::analyze(bytes, Delimiter::Comma, enc).layout;
        let mut doc = Document::new(bytes, &layout, Delimiter::Comma, enc);
        let clear = Edit::SetCell {
            row,
            column: 0,
            value: String::new(),
        };
        doc.apply(&clear).unwrap();
        let saved = doc.save().unwrap();
        // The save asks for a hint, and with it the reopen is right.
        assert_eq!(saved.encoding_hint, Some(enc));
        assert_eq!(reopen_encoding(&saved.bytes, saved.encoding_hint), enc);
        // Without it, the guess flips.
        (enc, reopen_encoding(&saved.bytes, None))
    };
    // Windows-1252 with one high byte (€) → pure ASCII → UTF-8. The text
    // decodes the same either way; only future edits would encode
    // differently.
    assert_eq!(
        flip(b"a\n\x80\n", 1),
        (Encoding::Windows1252, Encoding::Utf8)
    );
    // UTF-8 with 3 multibyte characters and 2 invalid bytes → 2 and 2, no
    // longer "outnumber" → Windows-1252. The remaining é now reads as Ã©.
    let utf8 = "é\néé,"
        .as_bytes()
        .iter()
        .chain(b"\xE2\x82\n")
        .copied()
        .collect::<Vec<u8>>();
    assert_eq!(flip(&utf8, 0), (Encoding::Utf8, Encoding::Windows1252));
    // With the hint it reopens as UTF-8 again, and its invalid bytes get the
    // usual warning rather than overriding the hint.
    let layout = oracle::analyze(&utf8, Delimiter::Comma, Encoding::Utf8).layout;
    let mut doc = Document::new(&utf8, &layout, Delimiter::Comma, Encoding::Utf8);
    let clear = Edit::SetCell {
        row: 0,
        column: 0,
        value: String::new(),
    };
    doc.apply(&clear).unwrap();
    let saved = doc.save().unwrap();
    let reopened = reopen_encoding(&saved.bytes, saved.encoding_hint);
    let reparsed = oracle::analyze(&saved.bytes, Delimiter::Comma, reopened).layout;
    let found = diagnostics::derive(&reparsed, &saved.bytes, reopened);
    assert!(
        found
            .iter()
            .any(|d| d.kind == DiagnosticKind::InvalidEncoding)
    );
    // Windows-1252 whose only non-UTF-8 byte is edited away, leaving bytes
    // that happen to be valid UTF-8 (C3 A9 is "Ã©" in Windows-1252) → UTF-8.
    // The remaining cell now reads as é.
    assert_eq!(
        flip(b"\xC3\xA9\n\x80\n", 1),
        (Encoding::Windows1252, Encoding::Utf8)
    );
}

/// ADR-0004 decision 4: a file whose last row is an unterminated quote has
/// no final line ending (the trailing LF is inside the quote), so deleting
/// that row leaves the new last row without one: `a\n"b\nc\n` → `a`.
#[test]
fn deleting_an_unterminated_last_row_drops_the_line_ending_before_it() {
    let bytes = b"a\n\"b\nc\n";
    let layout = oracle::analyze(bytes, Delimiter::Comma, Encoding::Utf8).layout;
    assert!(layout.rows[1].fields[0].unterminated);
    assert!(!layout.trailing_newline());
    let mut doc = Document::new(bytes, &layout, Delimiter::Comma, Encoding::Utf8);
    doc.apply(&Edit::DeleteRow { row: 1 }).unwrap();
    let saved = doc.save().unwrap();
    assert_eq!(saved.bytes, b"a");
    assert_eq!(apply_changes(bytes, &saved.changes), saved.bytes);
    assert_eq!(saved.line_endings, vec![None]);
}

/// Coverage over a fixed-seed sample: each ADR-0004 structure fix and each
/// edge operation must happen in at least ~1% of generated edit cases, so
/// the properties above really exercise them.
#[test]
fn every_save_rule_and_edge_operation_is_exercised() {
    let counts = coverage_counts(&mut proptest::test_runner::TestRunner::deterministic());
    if let Err(e) = check_coverage(&counts) {
        panic!("{e}");
    }
}

/// The coverage floors hold for other seeds too, not just the deterministic
/// one, so a harmless change to a generator doesn't trip them. Slow, so it
/// runs on demand:
/// `cargo nextest run -p leal-testkit --run-ignored only --no-capture seeds`.
#[test]
#[ignore = "slow: checks the coverage floors over 40 seeds"]
fn coverage_floors_hold_for_many_seeds() {
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let mut all: std::collections::BTreeMap<&str, Vec<usize>> = std::collections::BTreeMap::new();
    let mut failures = Vec::new();
    for seed in 0..40_u8 {
        let rng = TestRng::from_seed(RngAlgorithm::ChaCha, &[seed; 32]);
        let counts = coverage_counts(&mut TestRunner::new_with_rng(Config::default(), rng));
        if let Err(e) = check_coverage(&counts) {
            failures.push(format!("seed {seed}: {}", e.lines().next().unwrap_or("")));
        }
        for (k, v) in counts {
            all.entry(k).or_default().push(v);
        }
    }
    for (k, v) in &all {
        let min = v.iter().min().copied().unwrap_or(0);
        let mean = v.iter().sum::<usize>() / v.len();
        println!(
            "{k}: min {min}, mean {mean} of {COVERAGE_CASES} ({} seeds)",
            v.len()
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Cases per coverage sample.
const COVERAGE_CASES: usize = 3000;

/// How many of [`COVERAGE_CASES`] edit cases from `runner` exercise each
/// save rule and edge operation.
fn coverage_counts(
    runner: &mut proptest::test_runner::TestRunner,
) -> std::collections::BTreeMap<&'static str, usize> {
    use proptest::strategy::ValueTree;

    let strategy = edit_case(CsvConfig::messy());
    let cases: Vec<EditCase> = (0..COVERAGE_CASES)
        .map(|_| strategy.new_tree(runner).unwrap().current())
        .collect();

    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for case in &cases {
        let mut seen: Vec<&str> = Vec::new();
        if let Ok(saved) = &case.saved {
            for fix in &saved.fixes {
                seen.push(match fix {
                    Fix::EmptyRowQuoted { .. } => "fix: empty row written as \"\"",
                    Fix::CrSplit { .. } => "fix: CR/LF split",
                    Fix::BomLikeQuoted => "fix: BOM-like first field quoted",
                });
                // Also counted per BOM, so that each kind is exercised. The
                // file has no BOM, so the output starts with the quote.
                if *fix == Fix::BomLikeQuoted {
                    seen.push(match &saved.bytes[1..] {
                        b if b.starts_with(UTF8_BOM) => "fix: BOM-like EF BB BF quoted",
                        b if b.starts_with(UTF16LE_BOM) => "fix: BOM-like FF FE quoted",
                        b if b.starts_with(UTF16BE_BOM) => "fix: BOM-like FE FF quoted",
                        _ => panic!("BomLikeQuoted without a BOM-like start: {case:?}"),
                    });
                }
            }
            if saved.encoding_hint.is_some() {
                seen.push("encoding hint written");
            }
        }
        // ADR-0004 decision 8: edits refused after an unterminated quote.
        if !case.refused.is_empty() {
            seen.push("edit: refused after an unterminated quote");
        }
        // Replay, looking at the document just before each edit.
        let mut doc = case.file.document();
        for e in &case.edits {
            let rows = doc.row_count();
            let cols = doc.max_row_len();
            // ADR-0005 decision 2: hatched cells, including a blank line's.
            if let Edit::SetCell { row, column, .. } = e
                && *column >= doc.row_len(*row)
            {
                seen.push("edit: a hatched cell");
                if doc.row_len(*row) == 1 && doc.value(*row, 0).as_deref() == Some("") {
                    seen.push("edit: a hatched cell of an empty one-field row");
                }
            }
            match e {
                Edit::SetCell { row, .. } if rows == 1 && *row == 0 => {
                    seen.push("edit: the only row")
                }
                Edit::SetCell { row, .. } if doc.row_len(*row) == 1 => {
                    seen.push("edit: the only column")
                }
                Edit::DeleteRow { .. } if rows == 1 => seen.push("delete: the only row"),
                Edit::DeleteRow { row } if *row == 0 => seen.push("delete: first row"),
                Edit::DeleteRow { row } if *row + 1 == rows => seen.push("delete: last row"),
                Edit::InsertRow { at, .. } if *at == 0 => seen.push("insert: first row"),
                Edit::InsertRow { at, .. } if *at == rows => {
                    seen.push("insert: after the last row")
                }
                Edit::DeleteColumn { .. } if cols == 1 => seen.push("delete: the only column"),
                Edit::DeleteColumn { column } if *column == 0 => seen.push("delete: first column"),
                Edit::DeleteColumn { column } if *column + 1 == cols => {
                    seen.push("delete: last column")
                }
                Edit::InsertColumn { at, .. } if *at == 0 => seen.push("insert: first column"),
                Edit::InsertColumn { at, .. } if *at == cols => {
                    seen.push("insert: after the last column")
                }
                _ => {}
            }
            doc.apply(e).unwrap();
        }
        seen.sort_unstable();
        seen.dedup();
        for s in seen {
            *counts.entry(s).or_default() += 1;
        }
    }
    counts
}

/// Checks the coverage floors, returning the first one missed.
///
/// Every floor is 1% (30 of 3000). Over 40 seeds
/// (`coverage_floors_hold_for_many_seeds`) the lowest count for any item
/// was 38 (the CR/LF split, mean 50), and the BOM-like kinds bottomed out at
/// 55 (`FF FE` and `FE FF`, mean 72) and 209 (`EF BB BF`, mean 244). Each
/// floor is at most 60% of its item's mean, so it fails only if a change
/// makes the item clearly rarer. Rerun that test after changing a generator.
fn check_coverage(counts: &std::collections::BTreeMap<&str, usize>) -> Result<(), String> {
    const N: usize = COVERAGE_CASES;
    let expected = [
        "fix: empty row written as \"\"",
        "fix: CR/LF split",
        "fix: BOM-like first field quoted",
        "fix: BOM-like EF BB BF quoted",
        "fix: BOM-like FF FE quoted",
        "fix: BOM-like FE FF quoted",
        "encoding hint written",
        "edit: the only row",
        "edit: the only column",
        "edit: a hatched cell",
        "edit: a hatched cell of an empty one-field row",
        "edit: refused after an unterminated quote",
        "delete: the only row",
        "delete: first row",
        "delete: last row",
        "insert: first row",
        "insert: after the last row",
        "delete: the only column",
        "delete: first column",
        "delete: last column",
        "insert: first column",
        "insert: after the last column",
    ];
    for name in expected {
        let n = counts.get(name).copied().unwrap_or(0);
        if n * 100 < N {
            return Err(format!("{name}: {n} of {N} cases, under 1%\n{counts:#?}"));
        }
    }
    Ok(())
}
