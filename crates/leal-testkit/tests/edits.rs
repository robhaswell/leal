//! Self-tests for the save oracle, the edit strategy and the UTF-16
//! generator, checked against the reference parser.

mod oracle;

use leal_testkit::dialect::{Bom, Encoding, decode_value};
use leal_testkit::fidelity::{apply_changes, check_identical};
use leal_testkit::save::{Edit, SaveError};
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
        // Skipped, as open questions for §3.7 (docs/tasks/0.2.md):
        // - a row with no bytes (no cells, or one empty cell) reads back as a
        //   blank line (one empty field), or at the very end as nothing;
        // - deleting rows can bring a field that starts with U+FEFF to the
        //   start of the file, where it reads as a BOM.
        let empty_row = values.iter().any(|r| r.len() < 2 && r.first().is_none_or(String::is_empty));
        let new_bom = Bom::detect(&saved.bytes) != Bom::detect(&case.file.bytes);
        if empty_row || new_bom {
            return Ok(());
        }

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
        .filter(|c| {
            let values = final_values(c);
            let empty_row = values
                .iter()
                .any(|r| r.len() < 2 && r.first().is_none_or(String::is_empty));
            let new_bom = c
                .saved
                .as_ref()
                .is_ok_and(|s| Bom::detect(&s.bytes) != Bom::detect(&c.file.bytes));
            !c.edits.is_empty() && !empty_row && !new_bom
        })
        .count();
    assert!(
        checked >= 250,
        "only {checked} of 500 edited cases are checked"
    );
}
