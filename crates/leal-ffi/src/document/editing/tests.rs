//! What Swift sees of editing: commands for its undo manager, batches,
//! refusals, the dirty state, the full value, lineage and replay.

use super::*;

use crate::document::tests::{TempDir, block_on, options};
use crate::{OpenOptions, Scheduler, VolumeInfo, open_document};

const FILE: &[u8] = b"id,name\n1,Marlow\n2\n3,\"open\nquote\n";

fn open(dir: &TempDir, scheduler: &Scheduler, name: &str) -> std::sync::Arc<Document> {
    let path = dir.file(name, FILE);
    let document = open_document(
        &path,
        VolumeInfo::default(),
        dir.locations(),
        scheduler,
        options(),
        None,
    )
    .unwrap();
    assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
    document
}

fn name(document: &Document, row: u64) -> Vec<String> {
    document.rows(row, 1, 100).unwrap()[0]
        .iter()
        .map(|cell| cell.text.clone())
        .collect()
}

fn refused(path: &str, refusal: EditRefusal, row: Option<u64>, column: Option<u32>) -> LealError {
    LealError::EditRefused {
        path: path.to_owned(),
        refusal,
        row,
        column,
    }
}

#[test]
fn an_edit_gives_a_command_that_undoes_and_redoes() {
    let dir = TempDir::new("edit-commands");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    assert!(!document.has_unsaved_edits().unwrap());

    let command = document.set_cell(1, 1, "Marlowe").unwrap().unwrap();
    assert_eq!(
        command,
        EditCommand {
            lineage: document.lineage().unwrap(),
            changes: vec![ValueChange {
                row: 1,
                column: 1,
                old_value: Some("Marlow".into()),
                new_value: Some("Marlowe".into()),
            }],
            structural: None,
            cells: None,
        }
    );
    assert!(document.has_unsaved_edits().unwrap());
    assert_eq!(document.edited_cell_count().unwrap(), 1);
    assert_eq!(name(&document, 1), ["1", "Marlowe"]);
    assert_eq!(
        document.full_value(1, 1).unwrap().as_deref(),
        Some("Marlowe")
    );
    // An unchanged value is no edit.
    assert_eq!(document.set_cell(1, 1, "Marlowe").unwrap(), None);

    document.undo(command.clone()).unwrap();
    assert_eq!(name(&document, 1), ["1", "Marlow"]);
    assert!(!document.has_unsaved_edits().unwrap());
    let path = dir.file("a.csv", FILE);
    assert_eq!(
        document.undo(command.clone()),
        Err(refused(&path, EditRefusal::ValueChanged, Some(1), Some(1)))
    );
    document.redo(command).unwrap();
    assert_eq!(name(&document, 1), ["1", "Marlowe"]);
}

#[test]
fn hatched_cells_header_cells_batches_and_refusals() {
    let dir = TempDir::new("edit-refusals");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let path = dir.file("a.csv", FILE);
    // A hatched cell of the short row 2 is missing: nil, not "".
    let hatched = document.set_cell(2, 1, "two").unwrap().unwrap();
    assert_eq!(hatched.changes[0].old_value, None);
    assert_eq!(name(&document, 2), ["2", "two"]);
    document.set_cell(0, 1, "Name").unwrap().unwrap();
    assert_eq!(name(&document, 0), ["id", "Name"]);
    assert_eq!(document.full_value(2, 5).unwrap().as_deref(), Some(""));
    assert_eq!(
        document.set_cell(3, 2, "x"),
        Err(refused(
            &path,
            EditRefusal::AfterUnterminatedQuote,
            Some(3),
            Some(2)
        ))
    );
    assert_eq!(
        document.can_edit(3, 2).unwrap(),
        Some(EditRefusal::AfterUnterminatedQuote)
    );
    assert_eq!(document.can_edit(3, 1).unwrap(), None);
    assert_eq!(
        document.set_cell(9, 0, "x"),
        Err(refused(&path, EditRefusal::NoSuchRow, Some(9), None))
    );
    assert_eq!(
        document.set_cell(1, u32::MAX, "x"),
        Err(refused(
            &path,
            EditRefusal::TooFarRight,
            Some(1),
            Some(u32::MAX)
        ))
    );

    // A batch: one command, all or nothing.
    let cell = |row, column, value: &str| CellEdit {
        row,
        column,
        value: value.to_owned(),
    };
    let paste = document
        .set_cells(vec![cell(1, 0, "a"), cell(2, 0, "b")])
        .unwrap()
        .unwrap();
    assert_eq!(paste.changes.len(), 2);
    assert_eq!(
        document.set_cells(vec![cell(1, 0, "c"), cell(3, 2, "d")]),
        Err(refused(
            &path,
            EditRefusal::AfterUnterminatedQuote,
            Some(3),
            Some(2)
        ))
    );
    assert_eq!(name(&document, 1), ["a", "Marlow"]);
    document.undo(paste).unwrap();
    assert_eq!(name(&document, 1), ["1", "Marlow"]);

    // While there are edits, another delimiter is refused; the header row
    // can change, and keeps the lineage.
    let semicolon = OpenOptions {
        delimiter: Some(crate::Delimiter::Semicolon),
        ..options()
    };
    assert_eq!(
        document.reinterpret(semicolon),
        Err(LealError::UnsavedEdits { path: path.clone() })
    );
    let lineage = document.lineage().unwrap();
    let no_header = OpenOptions {
        header: Some(false),
        ..options()
    };
    let screen = document.reinterpret(no_header).unwrap();
    assert_eq!(screen.rows[0][1].text, "Name");
    assert!(document.has_unsaved_edits().unwrap());
    assert_eq!(document.lineage().unwrap(), lineage);

    // Once they are undone, the delimiter can change, and the old commands
    // are refused as another lineage's.
    document.undo(hatched.clone()).unwrap();
    document.set_cell(0, 1, "name").unwrap().unwrap();
    document.reinterpret(semicolon).unwrap();
    assert_ne!(document.lineage().unwrap(), lineage);
    assert_eq!(
        document.redo(hatched),
        Err(refused(&path, EditRefusal::OtherLineage, None, None))
    );
    assert!(document.edit_conflicts().unwrap().is_empty());
}

#[test]
fn replay_recovers_the_edits_and_names_those_that_no_longer_apply() {
    let dir = TempDir::new("edit-replay");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let lineage = document.lineage().unwrap();
    let history = vec![
        document.set_cell(1, 1, "Marlowe").unwrap().unwrap(),
        document.set_cell(2, 2, "far").unwrap().unwrap(),
        EditCommand {
            lineage,
            changes: vec![ValueChange {
                row: 1,
                column: 0,
                old_value: Some("not what it holds".into()),
                new_value: Some("x".into()),
            }],
            structural: None,
            cells: None,
        },
    ];
    let fresh = open(&dir, &scheduler, "fresh.csv");
    let report = fresh.replay(history.clone()).unwrap();
    assert_eq!(
        report.refused,
        vec![RefusedCommand {
            index: 2,
            refusal: EditRefusal::ValueChanged,
        }]
    );
    assert_eq!(report.applied.len(), 2);
    let fresh_lineage = fresh.lineage().unwrap();
    assert!(report.applied.iter().all(|c| c.lineage == fresh_lineage));
    assert_eq!(report.applied[0].changes, history[0].changes);
    assert_eq!(name(&fresh, 1), ["1", "Marlowe"]);
    assert_eq!(name(&fresh, 2), ["2", "", "far"]);
    // The replayed commands undo in the fresh document.
    for command in report.applied.into_iter().rev() {
        fresh.undo(command).unwrap();
    }
    assert!(!fresh.has_unsaved_edits().unwrap());
}

/// Row inserts and deletes (task 2.4a): commands Swift undoes and redoes
/// like any other, logical rows everywhere, and the refusals.
#[test]
fn rows_inserted_and_deleted_undo_redo_and_replay() {
    let dir = TempDir::new("edit-rows");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let path = dir.file("a.csv", FILE);
    assert_eq!(document.can_change_rows().unwrap(), None);
    assert_eq!(document.row_count().unwrap(), 4);

    let insert = document
        .insert_rows(1, vec![vec!["0".into(), "Vane".into()]])
        .unwrap()
        .unwrap();
    assert!(insert.changes.is_empty());
    let rows = insert.structural.clone().unwrap();
    assert!(rows.inserts());
    assert_eq!((rows.first_row(), rows.row_count()), (1, 1));
    assert_eq!(document.row_count().unwrap(), 5);
    assert_eq!(name(&document, 1), ["0", "Vane"]);
    assert_eq!(name(&document, 2), ["1", "Marlow"]);
    // A cell of the inserted row is edited like any other.
    let edit = document.set_cell(1, 1, "Vale").unwrap().unwrap();
    assert_eq!(name(&document, 1), ["0", "Vale"]);

    let delete = document.delete_rows(2, 2).unwrap().unwrap();
    assert!(!delete.structural.clone().unwrap().inserts());
    assert_eq!(document.row_count().unwrap(), 3);
    assert_eq!(name(&document, 2), ["3", "open\nquote\n"]);
    assert!(document.has_unsaved_edits().unwrap());

    // Nothing may follow the unterminated quote's row (ADR-0004 decision 8).
    assert_eq!(
        document.can_insert_rows(3).unwrap(),
        Some(EditRefusal::AfterUnterminatedQuote)
    );
    assert_eq!(document.can_insert_rows(2).unwrap(), None);
    assert_eq!(
        document.insert_rows(3, vec![vec!["x".into()]]),
        Err(refused(
            &path,
            EditRefusal::AfterUnterminatedQuote,
            Some(3),
            Some(0)
        ))
    );
    assert_eq!(
        document.delete_rows(2, 5),
        Err(refused(&path, EditRefusal::NoSuchRow, Some(3), None))
    );

    // Duplicate Row (task 2.5a): an insert of copies after the rows,
    // refused (asked first, or made) where an insert would be.
    assert_eq!(document.can_duplicate_rows(0, 2).unwrap(), None);
    let duplicate = document.duplicate_rows(0, 2).unwrap().unwrap();
    let copies = duplicate.structural.clone().unwrap();
    assert!(copies.inserts());
    assert_eq!((copies.first_row(), copies.row_count()), (2, 2));
    assert_eq!(name(&document, 3), ["0", "Vale"]);
    document.undo(duplicate).unwrap();
    assert_eq!(
        document.can_duplicate_rows(2, 1).unwrap(),
        Some(EditRefusal::AfterUnterminatedQuote)
    );
    assert_eq!(
        document.can_duplicate_rows(2, 2).unwrap(),
        Some(EditRefusal::NoSuchRow)
    );
    // More than the limit (the whole file selected, say): refused.
    assert_eq!(duplicate_row_limit(), 10_000);
    assert_eq!(
        document.can_duplicate_rows(0, 10_001).unwrap(),
        Some(EditRefusal::TooManyRows)
    );
    assert_eq!(
        document.duplicate_rows(0, 10_001),
        Err(refused(&path, EditRefusal::TooManyRows, None, None))
    );
    assert_eq!(
        document.duplicate_rows(2, 1),
        Err(refused(
            &path,
            EditRefusal::AfterUnterminatedQuote,
            Some(3),
            Some(0)
        ))
    );

    // Undo, last first, back to the file; redo, and undo again.
    document.undo(delete.clone()).unwrap();
    assert_eq!(name(&document, 2), ["1", "Marlow"]);
    document.undo(edit.clone()).unwrap();
    document.undo(insert.clone()).unwrap();
    assert!(!document.has_unsaved_edits().unwrap());
    assert_eq!(document.row_count().unwrap(), 4);
    document.redo(insert.clone()).unwrap();
    document.redo(edit.clone()).unwrap();
    document.redo(delete.clone()).unwrap();
    assert_eq!(document.row_count().unwrap(), 3);
    assert_eq!(name(&document, 1), ["0", "Vale"]);

    // Replayed into a fresh document, by value: the same rows.
    let fresh = open(&dir, &scheduler, "b.csv");
    let report = fresh
        .replay(vec![insert.clone(), edit.clone(), delete.clone()])
        .unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.applied.len(), 3);
    assert_eq!(fresh.row_count().unwrap(), 3);
    for row in 0..3 {
        assert_eq!(name(&fresh, row), name(&document, row));
    }
    for command in report.applied.into_iter().rev() {
        fresh.undo(command).unwrap();
    }
    assert!(!fresh.has_unsaved_edits().unwrap());
}

/// Column inserts and deletes (task 2.4b): commands Swift undoes and redoes
/// like any other, and the refusals.
#[test]
fn columns_inserted_and_deleted_undo_redo_and_replay() {
    let dir = TempDir::new("edit-columns");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let path = dir.file("a.csv", FILE);
    assert_eq!(document.can_insert_column(1).unwrap(), None);

    let insert = document.insert_column(1, "x".into()).unwrap().unwrap();
    assert!(insert.changes.is_empty());
    let column = insert.structural.clone().unwrap();
    assert!(column.inserts() && column.is_column());
    // Every row with at least one cell: all four.
    assert_eq!((column.column(), column.row_count()), (Some(1), 4));
    assert_eq!(name(&document, 1), ["1", "x", "Marlow"]);
    assert_eq!(name(&document, 2), ["2", "x"]);
    // After the open quote, in its row: refused (ADR-0004 decision 8).
    assert_eq!(
        document.can_insert_column(3).unwrap(),
        Some(EditRefusal::AfterUnterminatedQuote)
    );
    assert_eq!(
        document.insert_column(4, "y".into()),
        Err(refused(&path, EditRefusal::NoSuchColumn, None, Some(4)))
    );
    assert_eq!(
        document.can_delete_column(3).unwrap(),
        Some(EditRefusal::NoSuchColumn)
    );
    let delete = document.delete_column(0).unwrap().unwrap();
    assert!(!delete.structural.clone().unwrap().inserts());
    assert_eq!(name(&document, 1), ["x", "Marlow"]);

    document.undo(delete.clone()).unwrap();
    document.undo(insert.clone()).unwrap();
    assert!(!document.has_unsaved_edits().unwrap());
    document.redo(insert.clone()).unwrap();
    document.redo(delete.clone()).unwrap();
    assert_eq!(name(&document, 2), ["x"]);

    // Replayed into a fresh document, by value: the same cells.
    let fresh = open(&dir, &scheduler, "b.csv");
    let report = fresh.replay(vec![insert, delete]).unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    for row in 0..4 {
        assert_eq!(name(&fresh, row), name(&document, row));
    }
    for command in report.applied.into_iter().rev() {
        fresh.undo(command).unwrap();
    }
    assert!(!fresh.has_unsaved_edits().unwrap());
}

/// What task 2.5.2's journal needs: an undo recorded as the command's
/// inverse replays as the undo did, for cell and structural commands alike;
/// and `cells` names the edited cells, for the grid's marks.
#[test]
fn an_undo_replays_as_the_inverse_and_cells_name_the_edits() {
    let dir = TempDir::new("edit-inverse");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let edit = document.set_cell(1, 1, "Marlowe").unwrap().unwrap();
    let inverse = inverse_command(edit.clone());
    assert_eq!(inverse.lineage, edit.lineage);
    assert_eq!(
        inverse.changes,
        [ValueChange {
            row: 1,
            column: 1,
            old_value: Some("Marlowe".into()),
            new_value: Some("Marlow".into()),
        }]
    );
    assert_eq!(inverse_command(inverse.clone()), edit);
    let edited: Vec<Vec<u32>> = document
        .cells(0, 3, 0, 3, 100)
        .unwrap()
        .into_iter()
        .map(|row| row.edited)
        .collect();
    assert_eq!(edited, [vec![], vec![1], vec![]]);

    let delete = document.delete_rows(2, 1).unwrap().unwrap();
    document.undo(delete.clone()).unwrap();

    // The journal: the edit, the delete, the delete's undo.
    let fresh = open(&dir, &scheduler, "b.csv");
    let journal = vec![edit, delete.clone(), inverse_command(delete)];
    let report = fresh.replay(journal).unwrap();
    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(fresh.row_count().unwrap(), 4);
    for row in 0..4 {
        assert_eq!(name(&fresh, row), name(&document, row));
    }
}

/// Paste and Clear (task 2.6): one command each, of their cells, undone
/// and redone like an edit; the limits and refusals reach Swift.
#[test]
fn paste_and_clear_give_one_command_each() {
    let dir = TempDir::new("edit-paste");
    let scheduler = Scheduler::new().unwrap();
    let document = open(&dir, &scheduler, "a.csv");
    let path = dir.file("a.csv", FILE);
    assert_eq!(document.can_paste().unwrap(), None);

    // A block from row 1, column 0: row 2 (short) gets a hatched cell.
    let pasting = document
        .paste(1, 1, 0, 1, 2, "A\tB\r\nC\tD\r\n".into(), LineEnding::Lf)
        .unwrap();
    assert_eq!((pasting.rows, pasting.columns), (2, 2), "the block's size");
    let paste = pasting.command.unwrap();
    assert_eq!(paste.changes.len(), 4);
    assert_eq!(name(&document, 1), ["A", "B"]);
    assert_eq!(name(&document, 2), ["C", "D"]);
    document.undo(paste.clone()).unwrap();
    assert!(!document.has_unsaved_edits().unwrap());
    document.redo(paste.clone()).unwrap();

    // Clear: the hatched cell goes back to missing.
    assert_eq!(document.can_clear_cells(1, 2, 0, 2).unwrap(), None);
    let clear = document.clear_cells(2, 1, 0, 2).unwrap().unwrap();
    assert_eq!(name(&document, 2), [""]);
    document.undo(clear).unwrap();
    assert_eq!(name(&document, 2), ["C", "D"]);

    // The refusals.
    assert_eq!(cell_batch_limit(), 100_000);
    assert_eq!(paste_byte_limit(), 32 << 20);
    assert_eq!(
        document.paste(1, 1, 1, 1, 2, "a\tb".into(), LineEnding::Lf),
        Err(refused(&path, EditRefusal::PastLastColumn, None, None))
    );
    assert_eq!(
        document.paste(2, 1, 0, 1, 2, "a\nb\nc".into(), LineEnding::Lf),
        Err(refused(&path, EditRefusal::PastLastRow, None, None))
    );
    assert_eq!(
        document.can_clear_cells(0, 100_001, 0, 1).unwrap(),
        Some(EditRefusal::TooManyCells)
    );
    assert_eq!(
        document.clear_cells(0, 50_001, 0, 2),
        Err(refused(&path, EditRefusal::TooManyCells, None, None))
    );
    assert_eq!(
        document.paste(0, 1, 0, 1, 2, "x".repeat((32 << 20) + 1), LineEnding::Lf),
        Err(refused(&path, EditRefusal::TooMuchText, None, None))
    );
    // Empty text pastes nothing; a line break in a value is the file's.
    let empty = document
        .paste(1, 1, 0, 1, 2, String::new(), LineEnding::Lf)
        .unwrap();
    assert_eq!((empty.command, empty.rows), (None, 0));
    document
        .paste(1, 1, 0, 1, 2, "\"two\nlines\"".into(), LineEnding::Crlf)
        .unwrap()
        .command
        .unwrap();
    assert_eq!(name(&document, 1)[0], "two\r\nlines");
    // Past the unterminated quote's cell (row 3, column 1).
    assert_eq!(
        document.paste(3, 1, 2, 1, 3, "after".into(), LineEnding::Lf),
        Err(refused(
            &path,
            EditRefusal::AfterUnterminatedQuote,
            Some(3),
            Some(2)
        ))
    );
}

/// A paste of more than `INLINE_CHANGES` cells stays in Rust as a
/// `CellBatch`: Swift sees where it is and its longest values, and undoes,
/// redoes and replays it like any command.
#[test]
fn a_large_batch_stays_in_rust() {
    let dir = TempDir::new("edit-batch");
    let scheduler = Scheduler::new().unwrap();
    let mut bytes = b"a,b\n".to_vec();
    for row in 0..400 {
        bytes.extend_from_slice(format!("{row},x\n").as_bytes());
    }
    let path = dir.file("a.csv", &bytes);
    let open_it = |path: &str| {
        let document = open_document(
            path,
            VolumeInfo::default(),
            dir.locations(),
            &scheduler,
            options(),
            None,
        )
        .unwrap();
        assert_eq!(block_on(document.index_job().unwrap().wait()), Ok(()));
        document
    };
    let document = open_it(&path);
    let text: Vec<String> = (0..300)
        .map(|row| format!("p{row}\tlonger value {row}"))
        .collect();
    let command = document
        .paste(1, 1, 0, 1, 2, text.join("\n"), LineEnding::Lf)
        .unwrap()
        .command
        .unwrap();
    assert!(command.changes.is_empty());
    let cells = command.cells.clone().expect("a batch");
    assert_eq!(cells.count(), 600);
    assert!(!cells.clears());
    assert_eq!(
        cells.rows(),
        Some(RowSpan {
            first: 1,
            last: 300
        })
    );
    assert_eq!(
        cells.first(),
        Some(ValueChange {
            row: 1,
            column: 0,
            old_value: Some("0".into()),
            new_value: Some("p0".into()),
        })
    );
    let longest = cells.longest(false, 2);
    assert_eq!(longest.len(), 4);
    assert!(
        longest
            .iter()
            .any(|change| change.new_value.as_deref() == Some("longer value 100"))
    );
    let undone = cells.longest(true, 1);
    assert_eq!(undone.len(), 2);
    assert_eq!(undone[1].old_value.as_deref(), Some("x"));

    document.undo(command.clone()).unwrap();
    assert_eq!(name(&document, 1), ["0", "x"]);
    document.redo(command.clone()).unwrap();
    assert_eq!(name(&document, 300), ["p299", "longer value 299"]);
    let inverse = inverse_command(command.clone());
    assert_eq!(inverse.cells.as_ref().map(|cells| cells.count()), Some(600));
    let cleared = document.clear_cells(1, 300, 0, 2).unwrap().unwrap();
    assert!(cleared.cells.as_ref().is_some_and(|cells| cells.clears()));
    document.undo(cleared).unwrap();

    let fresh = open_it(&dir.file("b.csv", &bytes));
    let report = fresh.replay(vec![command]).unwrap();
    assert!(report.refused.is_empty());
    assert!(report.applied[0].cells.is_some());
    assert_eq!(name(&fresh, 300), ["p299", "longer value 299"]);
}
