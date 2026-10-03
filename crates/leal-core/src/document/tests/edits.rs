//! Edits (task 2.1): the overlay, the commands, and every reader seeing
//! the cells as they read now (ADR-0008 decisions 2 to 5).

// A highlight's ranges are a list of `Range`s, often of one.
#![allow(clippy::single_range_in_vec_init)]

use super::*;

use crate::edit::{COLUMN_LIMIT, CellChange, Command, Edit, EditError};
use crate::find::Query;

mod properties;

/// A header row, then: an ordinary row, a short one, a blank line,
/// invalid UTF-8 in field 1, text after a closing quote in field 2, and
/// enough valid multibyte text that it reads as UTF-8.
const FILE: &[u8] = b"id,name,note\n\
1,Marlow,ok\n\
2,Ostrava\n\
\n\
3,caf\xE9,x\n\
4,a\"b,\"q\"z\n\
5,\xC3\xA9t\xC3\xA9,\xC3\xA9\n";

fn open_indexed(dir: &Dir, name: &str, bytes: &[u8]) -> Document {
    let (document, _) = open_bytes(dir, name, bytes);
    wait_for_index(&document);
    document
}

fn set(document: &Document, row: usize, column: usize, value: &str) -> Command {
    document
        .set_cell(row, column, value)
        .unwrap()
        .unwrap_or_else(|| panic!("({row}, {column}) already held {value:?}"))
}

fn row_text(document: &Document, row: usize) -> Vec<String> {
    document.rows(row..row + 1, 1000).unwrap()[0]
        .iter()
        .map(|cell| cell.text.clone())
        .collect()
}

fn change(command: &Command) -> &CellChange {
    let [change] = command.changes() else {
        panic!("one cell: {command:?}");
    };
    change
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "a command's value, which is optional"
)]
fn some(text: &str) -> Option<String> {
    Some(text.to_owned())
}

fn search(document: &Document, text: &str) -> Search {
    let search = document.find(&Query::new(text)).unwrap();
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    search
}

/// Every match, by stepping forward from the start until it wraps.
fn matches(search: &Search) -> Vec<(Place, u64)> {
    let mut places = Vec::new();
    let mut from = None;
    while let SearchStep::Found {
        place,
        ordinal,
        wrapped: false,
    } = search.step(from, true).unwrap()
    {
        assert_eq!(search.ordinal(place).unwrap(), Some(ordinal));
        places.push((place, ordinal));
        from = Some(place);
    }
    places
}

#[test]
fn an_edit_reads_everywhere_the_row_is_read() {
    let dir = Dir::new("edit-everywhere");
    let document = open_indexed(&dir, "a.csv", FILE);
    assert!(!document.has_edits());

    let command = set(&document, 1, 1, "Marlowe & Daughters");
    assert_eq!(
        change(&command),
        &CellChange {
            row: 1,
            column: 1,
            old: some("Marlow"),
            new: some("Marlowe & Daughters"),
        }
    );
    assert!(document.has_edits());
    assert_eq!(document.edited_cells(), 1);

    // The grid, a window of it, the inspector, the editor and Copy.
    assert_eq!(row_text(&document, 1), ["1", "Marlowe & Daughters", "ok"]);
    let window = document.cells(1..2, 1..2, 7).unwrap();
    assert_eq!(window[0].field_count, 3);
    assert_eq!(
        window[0].cells,
        [Cell {
            text: "Marlowe".into(),
            truncated: true
        }]
    );
    let value = document.cell_value(1, 1, 7).unwrap().unwrap();
    assert_eq!(
        (value.text.as_str(), value.truncated, value.characters),
        ("Marlowe", true, 19)
    );
    assert!(value.exists && !value.invalid);
    assert_eq!(
        document.full_value(1, 1).unwrap().as_deref(),
        Some("Marlowe & Daughters")
    );
    assert_eq!(
        document.copy_cells_now(1..2, 0..3).unwrap().as_deref(),
        Some("1\tMarlowe & Daughters\tok")
    );

    // Committing the value it already holds is no edit (ADR-0008
    // decision 3).
    assert!(
        document
            .set_cell(1, 1, "Marlowe & Daughters")
            .unwrap()
            .is_none()
    );
    // Back to the original removes the edit: still a command, since the
    // value changed.
    let back = set(&document, 1, 1, "Marlow");
    assert_eq!(change(&back).old, some("Marlowe & Daughters"));
    assert!(!document.has_edits());
    assert_eq!(document.edited_cells(), 0);
    assert_eq!(row_text(&document, 1), ["1", "Marlow", "ok"]);
}

/// An edit changes the overlay in place unless a reader holds it (a copy
/// in progress): with 100,000 edited rows, copying it on every edit made
/// one take 1.7 ms instead of under 1 µs (`edits/set_cell_100k_edited_rows`).
#[test]
fn an_edit_copies_the_overlay_only_while_a_reader_holds_it() {
    let dir = Dir::new("edit-in-place");
    let document = open_indexed(&dir, "a.csv", FILE);
    let store = Arc::clone(&document.current().edits);
    set(&document, 1, 1, "a");
    set(&document, 1, 2, "b");
    set(&document, 2, 1, "c");
    document
        .apply(&set(&document, 2, 2, "d").inverse())
        .unwrap();
    assert_eq!(store.copies(), 0, "changed in place");
    let held = store.overlay();
    set(&document, 1, 1, "e");
    assert_eq!(store.copies(), 1, "copied for the reader");
    assert_eq!(
        held.row(1).unwrap().get(crate::edit::CellId::Field(1)),
        Some("a"),
        "which keeps its own"
    );
}

#[test]
fn undo_and_redo_apply_a_command_and_its_inverse() {
    let dir = Dir::new("edit-undo");
    let document = open_indexed(&dir, "a.csv", FILE);
    let first = set(&document, 1, 2, "fine");
    let second = set(&document, 1, 2, "great");

    // Undo, in reverse order.
    document.apply(&second.inverse()).unwrap();
    assert_eq!(row_text(&document, 1)[2], "fine");
    document.apply(&first.inverse()).unwrap();
    assert_eq!(row_text(&document, 1)[2], "ok");
    assert!(!document.has_edits());
    // A command whose old value isn't the cell's no longer applies, and
    // changes nothing.
    assert!(matches!(
        document.apply(&second),
        Err(EditError::ValueChanged { row: 1, column: 2 })
    ));
    assert!(!document.has_edits());

    // Redo, in order.
    document.apply(&first).unwrap();
    document.apply(&second).unwrap();
    assert_eq!(row_text(&document, 1)[2], "great");
    assert!(matches!(
        document.apply(&second),
        Err(EditError::ValueChanged { .. })
    ));
}

/// ADR-0005 decision 2: a short or blank row's missing (hatched) cells can
/// be edited; the row then reads as long as its last edited cell.
#[test]
fn hatched_cells_of_short_and_blank_rows_can_be_edited() {
    let dir = Dir::new("edit-hatched");
    let document = open_indexed(&dir, "a.csv", FILE);
    let ragged = |row| document.row_flags(row..row + 1)[0];
    assert!(ragged(2).ragged && ragged(2).marked);

    // Filling in the short row's missing cell: no longer ragged.
    let fill = set(&document, 2, 2, "new");
    assert_eq!(change(&fill).old, None, "a hatched cell is missing");
    assert_eq!(row_text(&document, 2), ["2", "Ostrava", "new"]);
    assert_eq!(document.cells(2..3, 0..10, 100).unwrap()[0].field_count, 3);
    assert_eq!(ragged(2), RowFlags::default());

    // Past it: empty cells in between, and ragged again (5 fields, not 3).
    set(&document, 2, 4, "far");
    assert_eq!(row_text(&document, 2), ["2", "Ostrava", "new", "", "far"]);
    assert!(ragged(2).ragged);
    let padding = document.cell_value(2, 3, 100).unwrap().unwrap();
    assert!(padding.exists && padding.text.is_empty());
    assert!(!document.cell_value(2, 5, 100).unwrap().unwrap().exists);
    assert_eq!(document.full_value(2, 9).unwrap().as_deref(), Some(""));

    // Setting a hatched cell to "" is no edit; back to "" removes the
    // edit, and the padding before it.
    assert!(document.set_cell(2, 7, "").unwrap().is_none());
    set(&document, 2, 4, "");
    assert_eq!(row_text(&document, 2), ["2", "Ostrava", "new"]);
    set(&document, 2, 2, "");
    assert_eq!(row_text(&document, 2), ["2", "Ostrava"]);
    assert!(!document.has_edits());

    // A blank line edited in column 2 becomes a row of 3 fields.
    assert_eq!(row_text(&document, 3), [""]);
    set(&document, 3, 2, "x");
    assert_eq!(row_text(&document, 3), ["", "", "x"]);
    assert_eq!(ragged(3), RowFlags::default());
    set(&document, 3, 1, "y");
    set(&document, 3, 2, "");
    assert_eq!(row_text(&document, 3), ["", "y"]);
    assert!(ragged(3).ragged, "two fields, where most rows have three");

    // There is a limit to how far right.
    assert!(matches!(
        document.set_cell(1, COLUMN_LIMIT, "x"),
        Err(EditError::TooFarRight {
            row: 1,
            column: COLUMN_LIMIT
        })
    ));
}

/// ADR-0004 decision 8: an unterminated quote swallows the rest of the
/// file, so nothing may be written after it while it is open.
#[test]
fn edits_past_an_unterminated_quote_are_refused() {
    let dir = Dir::new("edit-unterminated");
    let document = open_indexed(&dir, "a.csv", b"a,b,c\n1,\"open\nmore\n");
    let original = document.full_value(1, 1).unwrap().unwrap();
    assert_eq!(original, "open\nmore\n");
    assert!(matches!(
        document.set_cell(1, 2, "x"),
        Err(EditError::AfterUnterminatedQuote { row: 1, column: 2 })
    ));
    assert!(!document.has_edits());
    // Editing the swallowed cell closes its quote; then a cell can follow.
    set(&document, 1, 1, "closed");
    set(&document, 1, 2, "after");
    // But not with the quote open again.
    assert!(matches!(
        document.set_cell(1, 1, &original),
        Err(EditError::AfterUnterminatedQuote { row: 1, column: 1 })
    ));
    set(&document, 1, 2, "");
    set(&document, 1, 1, &original);
    assert!(!document.has_edits());
    // Cells before it are fine.
    set(&document, 1, 0, "one");
    set(&document, 0, 3, "d");
}

/// The core edits file row 0 whether or not it is the header row: the
/// app decides whether to offer it (task 2.5). Find never searches it.
#[test]
fn the_header_row_can_be_edited() {
    let dir = Dir::new("edit-header");
    let document = open_indexed(&dir, "a.csv", FILE);
    assert!(document.detection().header);
    set(&document, 0, 1, "Ostrava name");
    assert_eq!(row_text(&document, 0), ["id", "Ostrava name", "note"]);
    let found = search(&document, "ostrava");
    assert_eq!(matches(&found), [(at(2, 1), 1)]);
}

#[test]
fn find_matches_edited_values_and_not_the_ones_they_replaced() {
    let dir = Dir::new("edit-find");
    let document = open_indexed(&dir, "a.csv", FILE);
    let before = search(&document, "ostrava");
    assert_eq!(matches(&before), [(at(2, 1), 1)]);

    // The edit replaces the only match: the finished search recounts.
    set(&document, 2, 1, "Prague");
    assert_eq!(before.progress().matches, 0);
    assert_eq!(before.step(None, true).unwrap(), SearchStep::NotFound);

    // Matches in edited values, which the row's bytes don't have, so the
    // raw-byte search alone wouldn't see them: a field, and a hatched cell.
    set(&document, 1, 2, "Ostrava too");
    set(&document, 3, 2, "OSTRAVA");
    let expected = [(at(1, 2), 1), (at(3, 2), 2)];
    assert_eq!(matches(&before), expected);
    assert_eq!(before.progress().matches, 2);
    let after = search(&document, "ostrava");
    assert_eq!(matches(&after), expected);
    assert_eq!(after.progress().matches, 2);
    assert_eq!(
        after.matches_in(0..10, 0..5, 100).unwrap(),
        [
            CellMatch {
                row: 1,
                column: 2,
                ranges: vec![0..7],
            },
            CellMatch {
                row: 3,
                column: 2,
                ranges: vec![0..7],
            },
        ]
    );
    // Previous, from past the end.
    assert_eq!(
        after.step(Some(at(9, 0)), false).unwrap(),
        found(3, 2, 2, false)
    );
    // An earlier match's count moves the later ones' numbers.
    set(&document, 1, 0, "ostrava");
    assert_eq!(
        matches(&after),
        [(at(1, 0), 1), (at(1, 2), 2), (at(3, 2), 3)]
    );
}

fn at(row: usize, column: usize) -> Place {
    Place { row, column }
}

fn found(row: usize, column: usize, ordinal: u64, wrapped: bool) -> SearchStep {
    SearchStep::Found {
        place: at(row, column),
        ordinal,
        wrapped,
    }
}

/// A search started during a run of edits catches up with them as it goes,
/// on a file large enough to take several chunks.
#[test]
fn a_running_search_keeps_up_with_edits() {
    let dir = Dir::new("edit-find-running");
    let bytes = sample(2 << 20);
    let document = open_indexed(&dir, "big.csv", &bytes);
    let rows = document.row_count();
    let search = document.find(&Query::new("needle")).unwrap();
    for row in (1..rows).step_by(rows / 50) {
        set(&document, row, 1, "a needle here");
    }
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    let edited: Vec<usize> = (1..rows).step_by(rows / 50).collect();
    let found: Vec<usize> = matches(&search).iter().map(|(p, _)| p.row).collect();
    assert_eq!(found, edited);
    assert_eq!(search.progress().matches, edited.len() as u64);
}

#[test]
fn a_copy_snapshots_the_edits_when_it_is_made() {
    let dir = Dir::new("edit-copy");
    let document = open_indexed(&dir, "a.csv", FILE);
    set(&document, 1, 1, "A");
    set(&document, 2, 3, "far");
    // The copy job is held before it reads a row (background work waits
    // while the user interacts), so the edit below certainly comes first.
    document.scheduler.set_interacting(true);
    let job = document.copy_cells(1..3, 0..4);
    // Made after the copy: not in it.
    set(&document, 1, 1, "B");
    std::thread::sleep(Duration::from_millis(20));
    assert!(!job.control().is_finished(), "the copy was held");
    document.scheduler.set_interacting(false);
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    let text = job.wait().unwrap().take().unwrap();
    assert_eq!(text, "1\tA\tok\t\n2\tOstrava\t\tfar");
    assert_eq!(
        document.copy_cells_now(1..2, 0..2).unwrap().as_deref(),
        Some("1\tB")
    );
}

/// ADR-0008 decision 2: an edited cell is checked on its new value, for
/// the gutter marks and each kind's Previous and Next.
#[test]
fn diagnostics_check_an_edited_cell_on_its_new_value() {
    let dir = Dir::new("edit-diagnostics");
    let document = open_indexed(&dir, "a.csv", FILE);
    let invalid = DiagnosticKind::InvalidEncoding;
    let after_quote = DiagnosticKind::TextAfterClosingQuote;
    let ragged = DiagnosticKind::RaggedRows;
    let nul = DiagnosticKind::NulBytes;
    assert_eq!(walk_forward(&document, invalid), [at(4, 1)]);
    assert_eq!(walk_forward(&document, after_quote), [at(5, 2)]);
    assert_eq!(walk_forward(&document, ragged), [at(2, 2)]);
    let marked = |document: &Document| {
        let mut rows = Vec::new();
        let mut from = 0;
        while let Some(row) = document.next_row_with_diagnostic(from) {
            assert!(document.row_has_diagnostic(row));
            rows.push(row);
            from = row + 1;
        }
        rows
    };
    assert_eq!(marked(&document), [2, 4, 5]);

    // Fixing the invalid bytes and the text after the quote.
    set(&document, 4, 1, "café");
    set(&document, 5, 2, "qz");
    assert!(walk_forward(&document, invalid).is_empty());
    assert!(walk_backward(&document, invalid).is_empty());
    assert!(walk_forward(&document, after_quote).is_empty());
    assert!(!document.row_has_diagnostic(4));
    // Editing another field of the row keeps its invalid bytes' mark.
    set(&document, 4, 1, "caf\u{FFFD}");
    assert_eq!(document.edited_cells(), 1, "the displayed value is no edit");
    set(&document, 4, 2, "y");
    assert_eq!(walk_forward(&document, invalid), [at(4, 1)]);
    assert_eq!(walk_backward(&document, invalid), [at(4, 1)]);
    set(&document, 4, 1, "café");

    // A NUL in a new value is one, in an edited field or a hatched cell.
    set(&document, 1, 0, "a\0b");
    set(&document, 2, 3, "\0");
    assert_eq!(walk_forward(&document, nul), [at(1, 0), at(2, 3)]);
    assert_eq!(walk_backward(&document, nul), [at(1, 0), at(2, 3)]);

    // Ragged as the rows read now: row 2 is 4 cells long, and the blank
    // line edited past its end has 2.
    set(&document, 3, 1, "x");
    assert_eq!(walk_forward(&document, ragged), [at(2, 3), at(3, 2)]);
    assert_eq!(walk_backward(&document, ragged), [at(2, 3), at(3, 2)]);
    assert_eq!(marked(&document), [1, 2, 3]);
    assert_eq!(
        document.row_flags(0..7),
        [
            RowFlags::default(),
            RowFlags {
                marked: true,
                ragged: false
            },
            RowFlags {
                marked: true,
                ragged: true
            },
            RowFlags {
                marked: true,
                ragged: true
            },
            RowFlags::default(),
            RowFlags::default(),
            RowFlags::default(),
        ]
    );
    let mut previous = Vec::new();
    let mut to = 7;
    while let Some(row) = document.previous_row_with_diagnostic(to) {
        previous.push(row);
        to = row;
    }
    assert_eq!(previous, [3, 2, 1]);
}

#[test]
fn number_detection_sees_edits() {
    let dir = Dir::new("edit-numbers");
    let document = open_indexed(&dir, "a.csv", b"a,b\n1,x\n2,y\n");
    assert_eq!(document.numeric_columns(10).unwrap(), [true, false]);
    set(&document, 1, 0, "one");
    set(&document, 1, 1, "3");
    set(&document, 2, 1, "4");
    set(&document, 2, 2, "5");
    assert_eq!(document.numeric_columns(10).unwrap(), [false, true, true]);
}

/// ADR-0008 decision 4.
#[test]
fn a_new_delimiter_or_encoding_is_refused_while_there_are_edits() {
    let dir = Dir::new("edit-reinterpret");
    let document = open_indexed(&dir, "a.csv", FILE);
    let edit = set(&document, 1, 1, "Marlowe");
    let semicolon = Choices {
        delimiter: Some(Delimiter::Semicolon),
        ..Choices::default()
    };
    let latin = Choices {
        encoding: Some(Encoding::Windows1252),
        ..Choices::default()
    };
    for choices in [semicolon, latin] {
        assert!(matches!(
            document.reinterpret(choices, 10, 100),
            Err(DocumentError::UnsavedEdits)
        ));
        assert_eq!(document.generation(), 0, "the document is unchanged");
        assert_eq!(row_text(&document, 1), ["1", "Marlowe", "ok"]);
    }

    // The header row can change, and the edits stay, first screen included.
    let no_header = Choices {
        header: Some(false),
        ..Choices::default()
    };
    let screen = document.reinterpret(no_header, 10, 100).unwrap();
    assert!(!screen.detection.header);
    assert_eq!(screen.rows[1][1].text, "Marlowe");
    wait_for_index(&document);
    assert_eq!(row_text(&document, 1), ["1", "Marlowe", "ok"]);
    assert!(document.has_edits());
    // The same split again keeps them too.
    document.reinterpret(no_header, 10, 100).unwrap();
    assert!(document.has_edits());

    // Once no edits are left, the delimiter can change.
    document.apply(&edit.inverse()).unwrap();
    document.reinterpret(semicolon, 10, 100).unwrap();
    assert_eq!(row_text(&document, 1), ["1,Marlow,ok"]);
    // And the edits made before don't apply to the new split.
    assert!(matches!(
        document.apply(&edit),
        Err(EditError::OtherLineage)
    ));
}

/// ADR-0008 decision 4: a drive coming back keeps the edits, because the
/// file is confirmed unchanged.
#[test]
fn a_drive_coming_back_keeps_the_edits() {
    let dir = Dir::new("edit-reconnect");
    let bytes = sample(300 * 1024);
    let document = open_with_fault(&dir, &bytes, SimulatedFault::Disconnect { at: 150 * 1024 });
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );
    set(&document, 5, 1, "edited while away");
    // The index stopped on a read error, so rows past it aren't known to
    // be missing: they haven't been read.
    let unread = document.row_count() + 10;
    assert!(matches!(
        document.set_cell(unread, 0, "x"),
        Err(EditError::NotReadYet { .. })
    ));
    assert!(matches!(
        document.can_edit(unread, 0),
        Err(EditError::NotReadYet { .. })
    ));
    let lineage = document.lineage();
    document.source().simulate_drive_back();
    document.check_original();
    assert_eq!(document.generation(), 1);
    assert_eq!(document.lineage(), lineage, "the same edits");
    wait_for_index(&document);
    assert_eq!(row_text(&document, 5)[1], "edited while away");
    set(&document, unread, 0, "now it can be");
    assert_eq!(document.edited_cells(), 2);
}

#[test]
fn rows_not_read_yet_cant_be_edited_until_they_are() {
    let dir = Dir::new("edit-not-read");
    let bytes = sample(400 * 1024);
    let path = dir.file("a.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(10),
        None,
    )
    .unwrap();
    // The first 64 KB can be edited before the index has them.
    set(&document, 3, 1, "early");
    let later = screen.row_count + 100;
    assert!(matches!(
        document.set_cell(later, 1, "x"),
        Err(EditError::NotReadYet { row }) if row == later
    ));
    gate.open();
    let summary = wait_for_index(&document);
    set(&document, later, 1, "later");
    assert_eq!(row_text(&document, 3)[1], "early");
    assert!(matches!(
        document.set_cell(summary.rows, 0, "x"),
        Err(EditError::NoSuchRow { .. })
    ));
}

/// ADR-0008 decision 5: a failed document's edits are replayed into a
/// fresh one, which says which no longer apply.
#[test]
fn replaying_commands_into_a_fresh_document_reports_what_no_longer_applies() {
    let dir = Dir::new("edit-replay");
    let document = open_indexed(&dir, "a.csv", FILE);
    let history = vec![
        set(&document, 1, 1, "Marlowe"),
        set(&document, 2, 3, "far"),
        set(&document, 6, 0, "six"),
        set(&document, 1, 1, "Marlowe & Co"),
    ];
    let edited: Vec<Vec<Cell>> = document.rows(0..7, 100).unwrap();

    // The same file: every command applies, and the cells are the same.
    let fresh = open_indexed(&dir, "same.csv", FILE);
    let replay = fresh.replay(&history);
    assert_eq!(replay.commands.len(), 4);
    assert!(replay.refused.is_empty());
    // They come back in the fresh document's lineage, ready to undo.
    assert!(replay.commands.iter().all(|c| c.lineage == fresh.lineage()));
    assert_ne!(fresh.lineage(), document.lineage());
    assert!(matches!(
        fresh.apply(&history[3].inverse()),
        Err(EditError::OtherLineage)
    ));
    assert_eq!(fresh.rows(0..7, 100).unwrap(), edited);
    fresh.apply(&replay.commands[3].inverse()).unwrap();
    assert_eq!(row_text(&fresh, 1)[1], "Marlowe");

    // A changed file: row 1 is different and the last row is gone.
    let changed = b"id,name,note\n1,Marlow Foods,ok\n2,Ostrava\n\n3,x,x\n4,y,z\n";
    let other = open_indexed(&dir, "changed.csv", changed);
    let replay = other.replay(&history);
    assert_eq!(replay.commands.len(), 1);
    let refused: Vec<usize> = replay.refused.iter().map(|(i, _)| *i).collect();
    assert_eq!(refused, [0, 2, 3]);
    assert!(matches!(
        replay.refused[0].1,
        EditError::ValueChanged { row: 1, column: 1 }
    ));
    assert!(matches!(
        replay.refused[1].1,
        EditError::NoSuchRow { row: 6 }
    ));
    assert_eq!(row_text(&other, 2), ["2", "Ostrava", "", "far"]);
}

/// M1: a command from another split doesn't apply, even where the cell
/// happens to hold its old value under the new one.
#[test]
fn a_command_from_another_split_is_refused() {
    let dir = Dir::new("edit-lineage");
    // Row 1 is `a`, `k`, `;k;` with commas, and `a,k,`, `k`, `` with
    // semicolons: cell (1, 1) is `k` either way.
    let document = open_indexed(&dir, "a.csv", b"h\na,k,;k;\n");
    let edit = set(&document, 1, 1, "x");
    document.apply(&edit.inverse()).unwrap();
    let before = document.lineage();
    let semicolon = Choices {
        delimiter: Some(Delimiter::Semicolon),
        ..Choices::default()
    };
    document.reinterpret(semicolon, 10, 100).unwrap();
    wait_for_index(&document);
    assert_eq!(row_text(&document, 1), ["a,k,", "k", ""]);
    assert_ne!(document.lineage(), before);
    assert!(matches!(
        document.apply(&edit),
        Err(EditError::OtherLineage)
    ));
    assert!(!document.has_edits());
    // The header toggle keeps the lineage.
    let lineage = document.lineage();
    let no_header = Choices {
        header: Some(false),
        ..semicolon
    };
    document.reinterpret(no_header, 10, 100).unwrap();
    assert_eq!(document.lineage(), lineage);
}

/// M2: a missing cell is `None`, not `""`: undoing a hatched edit gives
/// the short row back as it was, and so does setting it back to `""`.
#[test]
fn a_missing_cell_is_not_an_empty_one() {
    let dir = Dir::new("edit-missing");
    let document = open_indexed(&dir, "a.csv", FILE);
    let far = set(&document, 2, 3, "far");
    assert_eq!(change(&far).old, None);
    let near = set(&document, 2, 2, "near");
    assert_eq!(change(&near).old, some(""), "padding before an edited cell");
    // Emptying the far cell makes it missing again: the row ends at `near`.
    let empty = set(&document, 2, 3, "");
    assert_eq!(change(&empty).new, None);
    assert_eq!(row_text(&document, 2), ["2", "Ostrava", "near"]);
    for command in [&empty, &near, &far] {
        document.apply(&command.inverse()).unwrap();
    }
    assert_eq!(row_text(&document, 2), ["2", "Ostrava"]);
    assert!(!document.has_edits());
    // A command that would make a field of the row missing doesn't apply.
    let odd = Command {
        lineage: document.lineage(),
        edit: Edit::SetCell(CellChange {
            row: 2,
            column: 1,
            old: some("Ostrava"),
            new: None,
        }),
    };
    assert!(matches!(
        document.apply(&odd),
        Err(EditError::ValueChanged { row: 2, column: 1 })
    ));
}

/// M3: a batch is one command, all or nothing, undone last first.
#[test]
fn a_batch_of_cells_is_one_command_all_or_nothing() {
    let dir = Dir::new("edit-batch");
    let document = open_indexed(&dir, "a.csv", b"a,b,c\n1,2,3\n4,5\n6,\"open\nquote\n");
    let paste = document
        .set_cells(&[(1, 0, "x"), (1, 1, "2"), (2, 2, "y"), (1, 0, "z")])
        .unwrap()
        .unwrap();
    // The unchanged cell is left out; the cell set twice is two changes.
    assert_eq!(paste.changes().len(), 3);
    assert_eq!(row_text(&document, 1), ["z", "2", "3"]);
    assert_eq!(row_text(&document, 2), ["4", "5", "y"]);
    assert_eq!(document.edited_cells(), 2);
    // A refusal anywhere: nothing applies, and it names the cell.
    let refused = document.set_cells(&[(1, 2, "ok"), (3, 2, "past the quote")]);
    assert!(matches!(
        refused,
        Err(EditError::AfterUnterminatedQuote { row: 3, column: 2 })
    ));
    assert!(matches!(
        document.set_cells(&[(1, 2, "ok"), (9, 0, "x")]),
        Err(EditError::NoSuchRow { row: 9 })
    ));
    assert_eq!(row_text(&document, 1), ["z", "2", "3"]);
    // Closing the quote in the same batch lets a cell follow it.
    let fix = document
        .set_cells(&[(3, 2, "after"), (3, 1, "closed")])
        .unwrap()
        .unwrap();
    document.apply(&fix.inverse()).unwrap();
    // Undo, redo and undo again.
    document.apply(&paste.inverse()).unwrap();
    assert!(!document.has_edits());
    document.apply(&paste).unwrap();
    assert_eq!(row_text(&document, 1), ["z", "2", "3"]);
    document.apply(&paste.inverse()).unwrap();
    assert_eq!(document.edited_cells(), 0);
    assert!(document.set_cells(&[(1, 1, "2")]).unwrap().is_none());
}

#[test]
fn can_edit_says_what_an_edit_would_be_refused_for() {
    let dir = Dir::new("edit-can");
    let document = open_indexed(&dir, "a.csv", b"a,b\n1,\"open\n");
    document.can_edit(0, 5).unwrap();
    document.can_edit(1, 1).unwrap();
    assert!(matches!(
        document.can_edit(1, 2),
        Err(EditError::AfterUnterminatedQuote { row: 1, column: 2 })
    ));
    assert!(matches!(
        document.can_edit(1, COLUMN_LIMIT),
        Err(EditError::TooFarRight { .. })
    ));
    assert!(matches!(
        document.can_edit(2, 0),
        Err(EditError::NoSuchRow { row: 2 })
    ));
    set(&document, 1, 1, "closed");
    document.can_edit(1, 2).unwrap();
}

#[test]
fn the_copy_estimate_counts_edited_values() {
    let dir = Dir::new("edit-estimate");
    let document = open_indexed(&dir, "a.csv", FILE);
    let before = document.estimated_copy_bytes(1..3, 0..3);
    set(&document, 1, 1, &"long ".repeat(200));
    set(&document, 2, 7, "outside the columns");
    assert_eq!(document.estimated_copy_bytes(1..3, 0..3), before + 1000);
}

/// F1: an edit made to a row of the first 64 KB, before the file turned
/// out to change while it was read, is kept, and named if the trusted copy
/// never reached the row. (The copy can't have the row with other bytes:
/// it refuses any chunk that differs from the first 64 KB,
/// `check_against_head`.) Rows the copy does have are fine.
#[test]
fn edits_on_rows_the_copy_never_reached_are_named() {
    let dir = Dir::new("edit-conflicts-unread");
    let bytes = sample(300 * 1024);
    let path = dir.file("usb.csv", &bytes);
    let source = Source::open_simulating_fault(
        &path,
        &dir.temp(),
        4096,
        Some(SimulatedFault::Change { at: 16 * 1024 }),
    )
    .unwrap();
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::from_source(source, &scheduler, options(30), None).unwrap();
    let late = screen.row_count - 10;
    set(&document, 2, 1, "early");
    set(&document, late, 1, "late");
    set(&document, late, 4, "past its end");
    assert!(document.edit_conflicts().is_empty(), "trusted so far");
    gate.open();
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::ChangedOnDisk)))
    );
    assert!(document.changed_on_disk());
    assert_eq!(document.edit_conflicts(), [(late, 1), (late, 4)]);
    assert_eq!(document.edited_cells(), 3, "kept");
    assert_eq!(row_text(&document, 2)[1], "early");
}

/// Task 2.1a: a drive that comes back without Leal's clone is read from the
/// user's file, though first paint read the clone. A same-size change to a
/// row of the first 64 KB, with its modification time put back, stops the
/// copy (`ChangedOnDisk`) rather than being read unnoticed, and an edit
/// made to that row from first paint's bytes is named as a conflict.
#[test]
fn a_drive_back_without_its_clone_catches_a_change_to_an_edited_head_row() {
    let dir = Dir::new("edit-reconnect-lost-clone");
    let bytes = sample(300 * 1024);
    // A record of the first 64 KB that the copy hasn't reached when the
    // drive goes, at 16 KB. Record `i` is row `i + 1`, after the header.
    let (i, at) = (0..)
        .find_map(|i| {
            let start = format!("\n{i},caf\u{e9} {i},");
            let at = memchr::memmem::find(&bytes, start.as_bytes())? + 1;
            (at > 40 * 1024).then_some((i, at))
        })
        .unwrap();
    let row = i + 1;
    let document = open_with_fault(&dir, &bytes, SimulatedFault::Disconnect { at: 16 * 1024 });
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::Disconnected)))
    );
    assert_eq!(row_text(&document, row)[0], i.to_string());
    set(&document, 2, 1, "copied");
    set(&document, row, 1, "edited from first paint");

    assert!(document.source().simulate_clone_lost(), "read from a clone");
    // "café" becomes "CAFé": the same length, in place, its time put back.
    let path = dir.0.join("usb.csv");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let name = u64::try_from(at + i.to_string().len() + 1).unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, b"CAF", name).unwrap();
    file.set_modified(modified).unwrap();
    drop(file);

    document.source().simulate_drive_back();
    assert_eq!(document.check_original().state, OriginalState::Unchanged);
    assert_eq!(document.generation(), 1, "reconnected to the user's file");
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::ChangedOnDisk)))
    );
    assert!(document.changed_on_disk());
    assert!(!document.can_save());
    assert_eq!(document.edit_conflicts(), [(row, 1)]);
    assert_eq!(document.edited_cells(), 2, "kept");
    assert_eq!(row_text(&document, 2)[1], "copied");
}

/// T1: a chunk searched before an edit and added after it is recounted:
/// `found.synced` drops back to the chunk's version.
#[test]
fn a_chunk_searched_before_an_edit_is_recounted_after_it() {
    let dir = Dir::new("edit-find-chunk");
    let document = Arc::new(open_indexed(&dir, "a.csv", FILE));
    // The job is held while the hook is set (background work waits while
    // the user interacts), so its one chunk runs the hook.
    document.scheduler.set_interacting(true);
    let search = Arc::new(document.find(&Query::new("needle")).unwrap());
    let hook_document = Arc::clone(&document);
    let hook_search = Arc::downgrade(&search);
    let ran = Arc::new(AtomicBool::new(false));
    let hook_ran = Arc::clone(&ran);
    search.in_chunk(move || {
        // Between the chunk's search and its adding: an edit, and a query
        // that catches up with it. The row isn't searched yet, so there is
        // nothing to recount, but the counts are then up to date with it.
        hook_document.set_cell(4, 0, "a needle").unwrap();
        if let Some(search) = hook_search.upgrade() {
            let _ = search.progress();
        }
        hook_ran.store(true, Ordering::Release);
    });
    document.scheduler.set_interacting(false);
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    assert!(ran.load(Ordering::Acquire), "the hook ran");
    assert_eq!(search.progress().matches, 1);
    assert_eq!(matches(&search), [(at(4, 0), 1)]);
}

/// T2: rows far into a multi-megabyte file (past the first screen, past
/// the first 64 KB, and in later copy and search chunks) read back edited
/// through every reader.
#[test]
fn edits_far_into_a_large_file_read_back_everywhere() {
    let dir = Dir::new("edit-large");
    let bytes = sample(4 << 20);
    let document = open_indexed(&dir, "big.csv", &bytes);
    let rows = document.row_count();
    let edited = [5, 3_000, rows / 2, rows - 2];
    for &row in &edited {
        set(&document, row, 1, "needle\0here");
    }
    for &row in &edited {
        assert_eq!(row_text(&document, row)[1], "needle\0here");
        let cells = document.cells(row..row + 1, 1..2, 100).unwrap();
        assert_eq!(cells[0].cells[0].text, "needle\0here");
        assert!(document.row_has_diagnostic(row), "a NUL, row {row}");
    }
    let found = search(&document, "needle");
    let places: Vec<Place> = matches(&found).iter().map(|&(p, _)| p).collect();
    assert_eq!(places, edited.map(|row| at(row, 1)));
    assert_eq!(
        walk_forward(&document, DiagnosticKind::NulBytes),
        edited.map(|row| at(row, 1))
    );
    let job = document.copy_cells(1..rows, 1..2);
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    let text = job.wait().unwrap().take().unwrap();
    let lines: Vec<&str> = text.split('\n').collect();
    for &row in &edited {
        assert_eq!(lines[row - 1], "needle\0here", "row {row}");
    }
}

/// S1: a finished search with many matching rows doesn't recount edits on
/// the caller's thread: `progress` says it is catching up, a job does it,
/// and the counts come right.
#[test]
fn many_edits_to_a_large_search_are_counted_in_the_background() {
    let dir = Dir::new("edit-find-many");
    let bytes = sample(4 << 20);
    let document = open_indexed(&dir, "big.csv", &bytes);
    let rows = document.row_count();
    let found = search(&document, "caf");
    let before = found.progress().matches;
    assert!(before > (1 << 16), "{before} matches");
    let edits: Vec<(usize, usize, String)> = (1..rows)
        .step_by(37)
        .map(|row| (row, 1, format!("{row}")))
        .collect();
    let cells: Vec<(usize, usize, &str)> =
        edits.iter().map(|(r, c, v)| (*r, *c, v.as_str())).collect();
    document.set_cells(&cells).unwrap().unwrap();
    // Too much for the caller's thread: left to a job.
    assert!(found.progress().catching_up);
    let deadline = std::time::Instant::now() + LONG;
    while found.progress().catching_up {
        assert!(std::time::Instant::now() < deadline, "never caught up");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(found.progress().matches, before - edits.len() as u64);
    assert!(matches!(
        found.step(None, true).unwrap(),
        SearchStep::Found { .. }
    ));
}

/// A catch-up job checkpoints, so it waits while the user interacts, and
/// dropping the search cancels it rather than leaving it to hold the
/// reading (DESIGN §3.10 rule 3).
#[test]
fn a_catch_up_job_pauses_for_the_user_and_ends_with_its_search() {
    let dir = Dir::new("edit-find-catch-up-job");
    let bytes = sample(4 << 20);
    let document = open_indexed(&dir, "big.csv", &bytes);
    let rows = document.row_count();
    let found = search(&document, "caf");
    let edits: Vec<(usize, usize, String)> = (1..rows)
        .step_by(37)
        .map(|row| (row, 1, format!("{row}")))
        .collect();
    let cells: Vec<(usize, usize, &str)> =
        edits.iter().map(|(r, c, v)| (*r, *c, v.as_str())).collect();
    document.set_cells(&cells).unwrap().unwrap();
    document.scheduler.set_interacting(true);
    assert!(found.progress().catching_up);
    let job = found.catch_up_job().expect("a catch-up job");
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !job.control().is_finished(),
        "held while the user interacts"
    );
    assert!(found.progress().catching_up);
    drop(found);
    assert_eq!(
        job.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    document.scheduler.set_interacting(false);
}

/// With no edits since, a search never says it is catching up, and its
/// steps are never `Pending` for it.
#[test]
fn a_search_with_no_edits_since_is_never_catching_up() {
    let dir = Dir::new("edit-find-no-edits");
    let document = open_indexed(&dir, "a.csv", FILE);
    let searching = search(&document, "ostrava");
    assert!(!searching.progress().catching_up);
    assert!(searching.catch_up_job().is_none());
    assert_eq!(searching.step(None, true).unwrap(), found(2, 1, 1, false));
    set(&document, 2, 1, "Prague");
    assert!(
        !searching.progress().catching_up,
        "a few rows: caught up at once"
    );
    assert_eq!(searching.step(None, true).unwrap(), SearchStep::NotFound);
}

/// A batch that leaves every cell as it found it is no edit.
#[test]
fn a_batch_that_ends_where_it_started_is_no_edit() {
    let dir = Dir::new("edit-batch-no-op");
    let document = open_indexed(&dir, "a.csv", FILE);
    assert!(
        document
            .set_cells(&[(1, 1, "x"), (2, 4, "far"), (1, 1, "Marlow"), (2, 4, "")])
            .unwrap()
            .is_none()
    );
    assert!(!document.has_edits());
    // Part of one is still an edit.
    let partial = document
        .set_cells(&[(1, 1, "x"), (1, 2, "y"), (1, 1, "Marlow")])
        .unwrap()
        .unwrap();
    assert_eq!(partial.changes().len(), 3);
    assert_eq!(document.edited_cells(), 1);
}

/// A row the index never reached because the file changed while it was
/// read can't be read: the refusal says so, not "not yet".
#[test]
fn a_row_lost_to_a_change_while_reading_is_unreadable() {
    let dir = Dir::new("edit-changed-row");
    let bytes = sample(300 * 1024);
    let document = open_with_fault(&dir, &bytes, SimulatedFault::Change { at: 150 * 1024 });
    assert_eq!(
        document.index_job().control().wait_timeout(LONG),
        Some(Err(JobError::Read(ReadErrorKind::ChangedOnDisk)))
    );
    let lost = document.row_count() + 10;
    assert!(matches!(
        document.set_cell(lost, 0, "x"),
        Err(EditError::Read { row, ref error })
            if row == lost && error.kind() == ReadErrorKind::ChangedOnDisk
    ));
}

/// A hatched edit far past its row's end costs what the row's own cells
/// do in Find and the marks, not a cell per column up to it.
#[test]
fn a_far_hatched_edit_is_found_and_marked_without_walking_every_column() {
    let dir = Dir::new("edit-far");
    let document = open_indexed(&dir, "a.csv", FILE);
    let far = COLUMN_LIMIT - 1;
    set(&document, 1, far, "far\0needle");
    let found = search(&document, "needle");
    assert_eq!(matches(&found), [(at(1, far), 1)]);
    assert_eq!(
        walk_forward(&document, DiagnosticKind::NulBytes),
        [at(1, far)]
    );
    let window = document.cells(1..2, far - 1..far + 1, 100).unwrap();
    assert_eq!(window[0].field_count, far + 1);
    assert_eq!(window[0].cells[1].text, "far\0needle");
}
