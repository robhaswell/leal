//! Self-tests for the save oracle, the edit strategy and the UTF-16
//! generator, checked against the reference parser.

mod oracle;

use leal_testkit::dialect::{
    Bom, Delimiter, Encoding, decode_value, expected_encoding, reopen_encoding,
};
use leal_testkit::fidelity::{Change, apply_changes, check_identical};
use leal_testkit::layout::Layout;
use leal_testkit::save::{Document, Edit, SaveError};
use leal_testkit::strategies::csv::{CsvConfig, csv_file, csv_file_utf16};
use leal_testkit::strategies::edits::{EditCase, edit_case, edits_for};
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
                    prop_assert!(values[r][c].contains('😀'), "{:?}", values[r][c]);
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
    // property really tests it.
    assert!(
        cases
            .iter()
            .any(|c| c.saved.as_ref().is_ok_and(|s| s.encoding_hint.is_some()))
    );
    // Some saves change exactly one field.
    assert!(
        cases
            .iter()
            .any(|c| c.saved.as_ref().is_ok_and(|s| s.changes.len() == 1
                && c.edits.len() == 1
                && matches!(c.edits[0], Edit::SetCell { .. })))
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
            // added inside it. (Row counts elsewhere can still change; see
            // `deleting_a_row_can_join_cr_and_lf` in src/save.rs.)
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
        // written only when it is needed (the file had none to update).
        let guess = expected_encoding(&saved.bytes);
        prop_assert_eq!(
            saved.encoding_hint,
            (guess != case.file.encoding).then_some(case.file.encoding)
        );
        prop_assert_eq!(reopen_encoding(&saved.bytes, saved.encoding_hint), case.file.encoding);

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
    // Windows-1252 whose only non-UTF-8 byte is edited away, leaving bytes
    // that happen to be valid UTF-8 (C3 A9 is "Ã©" in Windows-1252) → UTF-8.
    // The remaining cell now reads as é.
    assert_eq!(
        flip(b"\xC3\xA9\n\x80\n", 1),
        (Encoding::Windows1252, Encoding::Utf8)
    );
}
