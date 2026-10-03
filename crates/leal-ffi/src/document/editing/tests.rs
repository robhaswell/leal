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
