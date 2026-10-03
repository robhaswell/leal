//! Column inserts and deletes (task 2.4b): every reader through the
//! layout, undo and redo by identity and by value, Find starting again,
//! per-column quoting's census, and the edge cases of `docs/tasks/2.4.md`
//! §7.

use super::*;

use crate::edit::{CellId, Command, EditError};
use crate::find::Query;

/// A short (ragged) row, a blank line, and a header.
const FILE: &[u8] = b"name,n,x\na,1,p\nb\n\nc,3,q\n";

fn open_with(dir: &Dir, name: &str, bytes: &[u8], header: bool) -> Document {
    let path = dir.file(name, bytes);
    let options = OpenOptions {
        choices: Choices {
            header: Some(header),
            ..Choices::default()
        },
        ..options(10)
    };
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options,
        None,
    )
    .unwrap();
    wait_for_index(&document);
    document
}

fn texts(document: &Document) -> Vec<Vec<String>> {
    document
        .rows(0..document.row_count(), 1000)
        .unwrap()
        .into_iter()
        .map(|row| row.into_iter().map(|cell| cell.text).collect())
        .collect()
}

fn rows_of(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|&value| value.to_owned()).collect())
        .collect()
}

fn insert(document: &Document, at: usize, value: &str) -> Command {
    document.insert_column(at, value).unwrap().unwrap()
}

fn delete(document: &Document, at: usize) -> Command {
    document.delete_column(at).unwrap().unwrap()
}

fn set_cell(document: &Document, row: usize, column: usize, value: &str) -> Command {
    document.set_cell(row, column, value).unwrap().unwrap()
}

/// Row `row`'s cell ids, as the writer will see them.
fn ids(document: &Document, row: usize) -> Vec<CellId> {
    let reading = document.current();
    Document::read_rows_of(&reading, row..row + 1, |view| {
        (0..view.len()).filter_map(|c| view.cell_id(c)).collect()
    })
    .unwrap()
    .pop()
    .unwrap()
}

fn same_shape(document: &Document, row: usize) -> bool {
    let reading = document.current();
    Document::read_rows_of(&reading, row..row + 1, |view| view.same_shape())
        .unwrap()
        .pop()
        .unwrap()
}

#[test]
fn a_column_insert_and_delete_follow_rule_6() {
    let dir = Dir::new("columns-rule-6");
    let document = open_with(&dir, "a.csv", FILE, true);
    assert_eq!(document.column_count(), 3);
    // At 2: every row with at least 2 cells; not the short row or the
    // blank line. The header row is a row like any other.
    let inserted = insert(&document, 2, "new");
    assert!(inserted.is_structural() && inserted.is_column());
    assert_eq!(
        texts(&document),
        rows_of(&[
            &["name", "n", "new", "x"],
            &["a", "1", "new", "p"],
            &["b"],
            &[""],
            &["c", "3", "new", "q"],
        ])
    );
    assert_eq!(document.column_count(), 4);
    assert_eq!(
        ids(&document, 1),
        [
            CellId::Field(0),
            CellId::Field(1),
            CellId::Inserted(crate::edit::OpId(0)),
            CellId::Field(2)
        ]
    );
    assert!(!same_shape(&document, 1));
    assert!(same_shape(&document, 2), "the short row is as it was");
    // The marks: the short row is ragged against the new column count.
    let flags = document.row_flags(0..5);
    assert!(flags[2].ragged && !flags[3].ragged && !flags[1].ragged);
    // A delete at 0: every row with a cell there, not the blank line.
    let deleted = delete(&document, 0);
    assert_eq!(
        texts(&document),
        rows_of(&[
            &["n", "new", "x"],
            &["1", "new", "p"],
            &[],
            &[""],
            &["3", "new", "q"]
        ])
    );
    document.apply(&deleted.inverse()).unwrap();
    document.apply(&inserted.inverse()).unwrap();
    assert_eq!(
        texts(&document),
        rows_of(&[
            &["name", "n", "x"],
            &["a", "1", "p"],
            &["b"],
            &[""],
            &["c", "3", "q"]
        ])
    );
    assert!(!document.has_edits());
    assert!(same_shape(&document, 1));
    // Redo by identity.
    document.apply(&inserted).unwrap();
    assert_eq!(document.full_value(4, 2).unwrap().as_deref(), Some("new"));
}

#[test]
fn past_the_widest_row_is_refused() {
    let dir = Dir::new("columns-widest");
    let document = open_with(&dir, "a.csv", FILE, false);
    assert!(matches!(
        document.insert_column(4, "x"),
        Err(EditError::NoSuchColumn { column: 4 })
    ));
    assert!(matches!(
        document.delete_column(3),
        Err(EditError::NoSuchColumn { column: 3 })
    ));
    assert!(document.can_insert_column(4).is_err());
    assert!(document.can_insert_column(3).is_ok());
    assert!(document.can_delete_column(2).is_ok());
    // Just past the widest row: a column after the last, in the rows that
    // long.
    insert(&document, 3, "end");
    assert_eq!(texts(&document)[0], ["name", "n", "x", "end"]);
    assert_eq!(texts(&document)[2], ["b"]);
    assert!(!document.has_edits() || document.edited_cells() == 0);
}

#[test]
fn edits_follow_their_cells_and_undo_puts_back_the_bytes() {
    let dir = Dir::new("columns-edits");
    let document = open_with(&dir, "a.csv", FILE, false);
    let field = set_cell(&document, 1, 2, "P");
    // A hatched cell of the short row, two past its end.
    let hatched = set_cell(&document, 2, 2, "h");
    assert_eq!(texts(&document)[2], ["b", "", "h"]);
    let inserted = insert(&document, 1, "i");
    assert_eq!(texts(&document)[1], ["a", "i", "1", "P"]);
    // The short row now has 3 cells (its hatched ones count), so it gets
    // one too, before its padding.
    assert_eq!(texts(&document)[2], ["b", "i", "", "h"]);
    let edit_inserted = set_cell(&document, 1, 1, "I");
    // Deleting the column with an edited field hides it; undo shows it.
    let deleted = delete(&document, 3);
    assert_eq!(texts(&document)[1], ["a", "I", "1"]);
    // The short row's hatched cell went, and the padding before it.
    assert_eq!(texts(&document)[2], ["b", "i"]);
    for command in [&deleted, &edit_inserted, &inserted, &hatched, &field] {
        document.apply(&command.inverse()).unwrap();
    }
    assert_eq!(
        texts(&document),
        rows_of(&[
            &["name", "n", "x"],
            &["a", "1", "p"],
            &["b"],
            &[""],
            &["c", "3", "q"]
        ])
    );
    assert!(!document.has_edits());
    assert_eq!(document.edited_cells(), 0);
}

#[test]
fn every_column_can_be_deleted() {
    let dir = Dir::new("columns-all");
    let document = open_with(&dir, "a.csv", b"a,b\nc,d\n", false);
    let first = delete(&document, 0);
    let second = delete(&document, 0);
    assert_eq!(texts(&document), [Vec::<String>::new(), Vec::new()]);
    assert_eq!(document.column_count(), 0);
    assert!(matches!(
        document.delete_column(0),
        Err(EditError::NoSuchColumn { .. })
    ));
    // A column inserted into rows with none.
    let again = insert(&document, 0, "z");
    assert_eq!(texts(&document), [vec!["z"], vec!["z"]]);
    for command in [&again, &second, &first] {
        document.apply(&command.inverse()).unwrap();
    }
    assert_eq!(texts(&document), [vec!["a", "b"], vec!["c", "d"]]);
    assert!(!document.has_edits());
}

#[test]
fn nothing_goes_after_an_open_unterminated_quote() {
    let dir = Dir::new("columns-quote");
    let document = open_with(&dir, "a.csv", b"a,b\nc,\"open\nquote", false);
    // After the quote, in its row: refused; before it, fine.
    assert!(matches!(
        document.insert_column(2, "x"),
        Err(EditError::AfterUnterminatedQuote { row: 1, column: 2 })
    ));
    assert!(document.can_insert_column(2).is_err());
    let before = insert(&document, 1, "x");
    assert_eq!(texts(&document)[1], ["c", "x", "open\nquote"]);
    // Deleting the quote's column lifts the rule.
    let gone = delete(&document, 2);
    let after = insert(&document, 2, "y");
    assert_eq!(texts(&document)[1], ["c", "x", "y"]);
    // Undoing the delete would put the quote back before a cell: refused.
    assert!(matches!(
        document.apply(&gone.inverse()),
        Err(EditError::ValueChanged { .. } | EditError::AfterUnterminatedQuote { .. })
    ));
    for command in [&after, &gone, &before] {
        document.apply(&command.inverse()).unwrap();
    }
    assert!(!document.has_edits());
}

#[test]
fn columns_cant_change_until_the_file_is_read() {
    let dir = Dir::new("columns-reading");
    let bytes = sample(400 * 1024);
    let path = dir.file("a.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(10),
        None,
    )
    .unwrap();
    assert!(matches!(
        document.insert_column(1, "x"),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.delete_column(1),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.can_insert_column(1),
        Err(EditError::StillReading)
    ));
    gate.open();
    wait_for_index(&document);
    delete(&document, 1);
}

#[test]
fn find_starts_again_after_a_column_insert_or_delete() {
    let dir = Dir::new("columns-find");
    let document = open_with(&dir, "a.csv", FILE, true);
    let query = Query::new("ne");
    let search = document.find(&query).unwrap();
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(search.progress().matches, 0);
    let settle = |search: &Search| {
        while search.progress().catching_up {
            if let Some(job) = search.catch_up_job() {
                let _ = job.control().wait_timeout(LONG);
            }
        }
    };
    let inserted = insert(&document, 1, "new");
    settle(&search);
    // In each data row long enough: rows 1, 2 and 4 (not the header, nor the
    // blank line).
    assert_eq!(search.progress().matches, 3);
    let fresh = document.find(&query).unwrap();
    assert_eq!(fresh.job().control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(fresh.progress().matches, 3);
    assert!(search.progress().complete);
    document.apply(&inserted.inverse()).unwrap();
    settle(&search);
    assert_eq!(search.progress().matches, 0);
}

#[test]
fn a_column_command_from_other_edits_applies_by_value() {
    let dir = Dir::new("columns-replay");
    let document = open_with(&dir, "a.csv", FILE, false);
    let edit = set_cell(&document, 1, 1, "one");
    let inserted = insert(&document, 1, "i");
    let deleted = delete(&document, 2);
    document.apply(&deleted.inverse()).unwrap();
    let history = vec![edit, inserted, deleted.clone(), deleted.inverse()];
    let fresh = open_with(&dir, "b.csv", FILE, false);
    let replay = fresh.replay(&history);
    assert!(replay.refused.is_empty(), "{:?}", replay.refused);
    assert_eq!(texts(&fresh), texts(&document));
    for command in replay.commands.iter().rev() {
        fresh.apply(&command.inverse()).unwrap();
    }
    assert!(!fresh.has_edits());
    // Without the cell edit, the deleted cells read otherwise: refused.
    let other = open_with(&dir, "c.csv", FILE, false);
    let replay = other.replay(&history[1..3]);
    assert_eq!(replay.commands.len(), 1);
    assert!(matches!(
        replay.refused[..],
        [(1, EditError::ValueChanged { row: 1, column: 2 })]
    ));
}

#[test]
fn quoting_census_judges_each_column_now() {
    let dir = Dir::new("columns-quoting");
    let bytes = b"\"id\",name,\"note\"\n\"1\",ann,\"x\"\n\n\"2\",\"bob\",\"\"\n";
    let document = open_with(&dir, "a.csv", bytes, false);
    let census = |document: &Document| {
        let reading = document.current();
        let overlay = reading.edits.overlay();
        crate::save::ColumnQuoting::census(&reading, &overlay).unwrap()
    };
    let now = census(&document);
    assert!(!now.every_field());
    assert!(now.quoted(0) && !now.quoted(1) && now.quoted(2) && !now.quoted(3));
    // A column inserted at 1 has no original fields: not quoted; the
    // others move with it. Edited fields count by their original bytes.
    insert(&document, 1, "q");
    set_cell(&document, 1, 2, "a");
    let now = census(&document);
    assert!(now.quoted(0) && !now.quoted(1) && !now.quoted(2) && now.quoted(3));
    // Deleting the rows with unquoted names makes that column quoted.
    document.delete_rows(0, 2).unwrap();
    assert!(census(&document).quoted(2));
}

/// The marks of unedited rows follow the columns: a deleted column takes a
/// flagged field's warning with it (the row is read to find out), and an
/// inserted value with a NUL marks every row it went into.
#[test]
fn marks_follow_inserted_and_deleted_columns() {
    let dir = Dir::new("columns-marks");
    let document = open_with(&dir, "a.csv", b"a,b\nc,d\0\ne,f\n", false);
    let marked = |document: &Document| -> Vec<bool> {
        document.row_flags(0..3).iter().map(|f| f.marked).collect()
    };
    assert_eq!(marked(&document), [false, true, false]);
    let deleted = delete(&document, 1);
    assert_eq!(marked(&document), [false, false, false]);
    assert!(!document.row_has_diagnostic(1));
    assert_eq!(document.next_row_with_diagnostic(0), None);
    assert_eq!(
        document
            .next_with_kind(DiagnosticKind::NulBytes, 0)
            .unwrap(),
        None
    );
    let inserted = insert(&document, 1, "n\0");
    assert_eq!(marked(&document), [true, true, true]);
    assert_eq!(document.next_row_with_diagnostic(1), Some(1));
    assert_eq!(document.previous_row_with_diagnostic(3), Some(2));
    assert_eq!(
        document
            .next_with_kind(DiagnosticKind::NulBytes, 2)
            .unwrap(),
        Some(Place { row: 2, column: 1 })
    );
    document.apply(&inserted.inverse()).unwrap();
    document.apply(&deleted.inverse()).unwrap();
    assert_eq!(marked(&document), [false, true, false]);
    assert_eq!(
        document
            .next_with_kind(DiagnosticKind::NulBytes, 0)
            .unwrap(),
        Some(Place { row: 1, column: 1 })
    );
}
