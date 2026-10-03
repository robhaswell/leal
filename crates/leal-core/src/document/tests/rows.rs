//! Row inserts and deletes (task 2.4a): every reader through the piece
//! list, undo and redo by identity and by value, Find catching up, and the
//! edge cases of `docs/tasks/2.4.md` §7.

use super::*;

use crate::edit::{Command, Edit, EditError};
use crate::find::Query;

const FILE: &[u8] = b"name,n\na,1\nb,2\nc,3\nd,4\n";

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

fn insert(document: &Document, at: usize, rows: &[&[&str]]) -> Command {
    document.insert_rows(at, &rows_of(rows)).unwrap().unwrap()
}

fn delete(document: &Document, at: usize, count: usize) -> Command {
    document.delete_rows(at, count).unwrap().unwrap()
}

fn search(document: &Document, text: &str) -> Search {
    let search = document.find(&Query::new(text)).unwrap();
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    search
}

/// Every match, stepping forward until it wraps, checking each ordinal,
/// and the same backward.
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
    let mut back = Vec::new();
    let mut to = None;
    while let SearchStep::Found {
        place,
        ordinal,
        wrapped,
    } = search.step(to, false).unwrap()
    {
        if wrapped && to.is_some() {
            break;
        }
        back.push((place, ordinal));
        to = Some(place);
    }
    back.reverse();
    assert_eq!(back, places, "backward");
    places
}

/// A search's answers: the count and every match.
fn answers(search: &Search) -> (u64, Vec<(Place, u64)>) {
    (search.progress().matches, matches(search))
}

#[test]
fn inserted_and_deleted_rows_read_everywhere() {
    let dir = Dir::new("rows-read");
    let document = open_with(&dir, "a.csv", FILE, false);
    let inserted = insert(&document, 2, &[&["x", "9"]]);
    let Edit::InsertRows { at: 2, rows } = &inserted.edit else {
        panic!("an insert: {inserted:?}");
    };
    assert_eq!(rows.len(), 1);
    delete(&document, 4, 2);
    let expected = [["name", "n"], ["a", "1"], ["x", "9"], ["b", "2"]];
    assert_eq!(texts(&document), expected);
    assert_eq!(document.row_count(), 4);
    assert_eq!(document.estimated_row_count(), 4);
    assert_eq!(document.progress().rows, 4);
    assert_eq!(document.column_count(), 2);
    let window = document.cells(1..4, 1..2, 100).unwrap();
    let cells: Vec<(usize, &str)> = window
        .iter()
        .map(|row| (row.field_count, row.cells[0].text.as_str()))
        .collect();
    assert_eq!(cells, [(2, "1"), (2, "9"), (2, "2")]);
    assert_eq!(document.full_value(2, 0).unwrap().as_deref(), Some("x"));
    let value = document.cell_value(2, 1, 10).unwrap().unwrap();
    assert!(value.exists && value.text == "9");
    let tsv = "name\tn\na\t1\nx\t9\nb\t2";
    assert_eq!(
        document.copy_cells_now(0..4, 0..2).unwrap().as_deref(),
        Some(tsv)
    );
    let job = document.copy_cells(0..9, 0..2);
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(job.wait().unwrap().take().as_deref(), Some(tsv));
    assert!(document.estimated_copy_bytes(0..4, 0..2) > 0);
    // An inserted row's cells are edited like any row's, past its end too.
    set_cell(&document, 2, 1, "8");
    set_cell(&document, 2, 3, "far");
    assert_eq!(texts(&document)[2], ["x", "8", "", "far"]);
    assert_eq!(document.edited_cells(), 2);
    assert!(document.row_flags(2..3)[0].ragged);
    // Reading the file again with the header toggled keeps the rows.
    let screen = document
        .reinterpret(
            Choices {
                header: Some(true),
                ..Choices::default()
            },
            10,
            100,
        )
        .unwrap();
    let first: Vec<Vec<&str>> = screen
        .rows
        .iter()
        .map(|row| row.iter().map(|cell| cell.text.as_str()).collect())
        .collect();
    assert_eq!(
        first,
        [
            vec!["name", "n"],
            vec!["a", "1"],
            vec!["x", "8", "", "far"],
            vec!["b", "2"]
        ]
    );
    assert_eq!(screen.row_count, 4);
    wait_for_index(&document);
    assert_eq!(texts(&document).len(), 4);
}

fn set_cell(document: &Document, row: usize, column: usize, value: &str) -> Command {
    document.set_cell(row, column, value).unwrap().unwrap()
}

/// Undo puts back the same rows, with their edits, so the file's bytes
/// come back (F3); undoing everything leaves no edits.
#[test]
fn undo_and_redo_restore_the_same_rows_and_edits() {
    let dir = Dir::new("rows-undo");
    let document = open_with(&dir, "a.csv", FILE, true);
    let version = document.edit_version();
    let mut done = vec![set_cell(&document, 2, 1, "edited")];
    done.push(insert(&document, 1, &[&["new"], &["rows"]]));
    done.push(set_cell(&document, 2, 0, "ROWS"));
    done.push(delete(&document, 2, 3));
    assert_eq!(document.edit_version(), version + 1 + 2 + 1 + 3);
    assert_eq!(
        texts(&document),
        [
            vec!["name", "n"],
            vec!["new"],
            vec!["c", "3"],
            vec!["d", "4"]
        ]
    );
    assert_eq!(document.edited_cells(), 0, "the edits went with their rows");
    // A deleted row's edit is undone with its row: undo, last first.
    let all = texts(&document);
    for command in done.iter().rev() {
        document.apply(&command.inverse()).unwrap();
    }
    assert!(!document.has_edits());
    assert_eq!(
        texts(&document),
        rows_of(&[
            &["name", "n"],
            &["a", "1"],
            &["b", "2"],
            &["c", "3"],
            &["d", "4"]
        ])
    );
    for command in &done {
        document.apply(command).unwrap();
    }
    assert_eq!(texts(&document), all);
    // Applied out of order, a row command no longer fits.
    assert!(matches!(
        document.apply(&done[1].inverse()),
        Err(EditError::ValueChanged { .. })
    ));
    assert!(matches!(
        document.apply(&done[3]),
        Err(EditError::NoSuchRow { .. })
    ));
    for command in done.iter().rev() {
        document.apply(&command.inverse()).unwrap();
    }
    assert!(!document.has_edits());
    // Inserting and deleting the same row is no edit either.
    let row = insert(&document, 5, &[&["e", "5"]]);
    delete(&document, 5, 1);
    assert!(!document.has_edits());
    assert_eq!(row.inverse().inverse(), row);
}

/// The header row is logical row 0 (§7): deleting it makes the next row
/// the header, and a row inserted at 0 becomes it. Find leaves it out.
#[test]
fn the_header_row_is_logical_row_zero() {
    let dir = Dir::new("rows-header");
    let document = open_with(&dir, "a.csv", b"n,v\nn1,v\nn2,v\n", true);
    let all_v = |document: &Document| search(document, "v").progress().matches;
    assert_eq!(all_v(&document), 2);
    let removed = delete(&document, 0, 1);
    assert_eq!(texts(&document)[0], ["n1", "v"]);
    assert_eq!(all_v(&document), 1, "n1 is the header now");
    let header = insert(&document, 0, &[&["head", "v"]]);
    assert_eq!(all_v(&document), 2);
    let found = search(&document, "n");
    assert_eq!(
        matches(&found)
            .iter()
            .map(|(p, _)| p.row)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    document.apply(&header.inverse()).unwrap();
    document.apply(&removed.inverse()).unwrap();
    assert!(!document.has_edits());
    // Above row 0 is allowed; so is appending.
    insert(&document, 0, &[&["top"]]);
    insert(&document, document.row_count(), &[&["end"]]);
    assert_eq!(texts(&document).first().unwrap(), &["top"]);
    assert_eq!(texts(&document).last().unwrap(), &["end"]);
}

/// A matching header row is left out of the summary's row count as well
/// as its match count.
#[test]
fn a_matching_header_row_isnt_counted_in_the_summary() {
    let dir = Dir::new("rows-header-summary");
    let document = open_with(&dir, "a.csv", b"name,marlow\nmarlow,x\ny,z\n", true);
    let search = search(&document, "marlow");
    let summary = *search.job().wait().unwrap();
    assert_eq!((summary.rows, summary.matches), (1, 1));
}

/// Every row can be deleted (§7): no rows, no columns, and nothing found.
#[test]
fn every_row_can_be_deleted() {
    let dir = Dir::new("rows-all");
    let document = open_with(&dir, "a.csv", FILE, false);
    let search_before = document.find(&Query::new("a")).unwrap();
    let all = delete(&document, 0, 5);
    assert_eq!(document.row_count(), 0);
    assert_eq!(document.column_count(), 0);
    assert!(texts(&document).is_empty());
    assert_eq!(
        document.copy_cells_now(0..5, 0..2).unwrap().as_deref(),
        Some("")
    );
    assert_eq!(search(&document, "a").progress().matches, 0);
    assert_eq!(
        search_before.job().control().wait_timeout(LONG),
        Some(Ok(()))
    );
    assert_eq!(search_before.progress().matches, 0);
    assert_eq!(document.next_row_with_diagnostic(0), None);
    assert!(matches!(
        document.set_cell(0, 0, "x"),
        Err(EditError::NoSuchRow { row: 0 })
    ));
    let back = insert(&document, 0, &[&[]]);
    assert_eq!(texts(&document), [[""]]);
    document.apply(&back.inverse()).unwrap();
    document.apply(&all.inverse()).unwrap();
    assert!(!document.has_edits());
    assert_eq!(search_before.progress().matches, 2);
}

/// Inserted rows are marked from their cells (ragged, or a NUL), deleted
/// rows are passed over, and each kind's Next and Previous agree.
#[test]
fn marks_follow_inserted_and_deleted_rows() {
    let dir = Dir::new("rows-marks");
    let bytes = b"a,b\n1,2\nshort\n3,4\nlong,x,y\n5,6\n";
    let document = open_with(&dir, "a.csv", bytes, false);
    let ragged = |document: &Document| {
        (0..document.row_count())
            .filter(|&row| document.row_flags(row..row + 1)[0].ragged)
            .collect::<Vec<_>>()
    };
    assert_eq!(ragged(&document), [2, 4]);
    delete(&document, 2, 1);
    insert(&document, 1, &[&["one"], &["n\0l", "x"]]);
    // a,b / one / n\0l,x / 1,2 / 3,4 / long,x,y / 5,6
    assert_eq!(ragged(&document), [1, 5]);
    let flags = document.row_flags(0..7);
    let marked: Vec<usize> = (0..7).filter(|&r| flags[r].marked).collect();
    assert_eq!(marked, [1, 2, 5]);
    let mut next = Vec::new();
    let mut from = 0;
    while let Some(row) = document.next_row_with_diagnostic(from) {
        next.push(row);
        from = row + 1;
    }
    assert_eq!(next, marked);
    assert_eq!(document.previous_row_with_diagnostic(7), Some(5));
    assert_eq!(document.previous_row_with_diagnostic(5), Some(2));
    assert!(document.row_has_diagnostic(2) && !document.row_has_diagnostic(3));
    assert_eq!(
        walk_forward(&document, DiagnosticKind::RaggedRows),
        [Place { row: 1, column: 1 }, Place { row: 5, column: 2 }]
    );
    assert_eq!(
        walk_backward(&document, DiagnosticKind::RaggedRows),
        [Place { row: 1, column: 1 }, Place { row: 5, column: 2 }]
    );
    assert_eq!(
        walk_forward(&document, DiagnosticKind::NulBytes),
        [Place { row: 2, column: 0 }]
    );
}

/// **Next** and **Previous** over many deleted and inserted rows search
/// the file's marks a constant number of times, not once per piece of the
/// row list (each search can run to the next mark anywhere in the file).
/// Counted by a hook, so it can't flake like a timing.
#[test]
fn marks_search_does_not_scale_with_the_pieces() {
    use crate::diagnostics::MARK_SEARCHES;
    let dir = Dir::new("rows-marks-cost");
    let mut bytes = b"a,b\nlone\n".to_vec();
    for _ in 0..400 {
        bytes.extend_from_slice(b"1,2\n");
    }
    bytes.extend_from_slice(b"end\n");
    let document = open_with(&dir, "a.csv", &bytes, false);
    // Every other plain row deleted, and a plain row inserted after each
    // pair: some 200 deleted and 100 inserted pieces.
    for row in (3..400).rev().step_by(2) {
        delete(&document, row, 1);
    }
    for row in (2..200).rev().step_by(2) {
        insert(&document, row, &[&["1", "2"]]);
    }
    let last = document.row_count() - 1;
    assert!(document.row_flags(1..2)[0].ragged);
    assert!(document.row_flags(last..last + 1)[0].ragged);
    let searches = |f: &dyn Fn() -> Option<usize>| {
        MARK_SEARCHES.with(|n| n.set(0));
        let row = f();
        (row, MARK_SEARCHES.with(std::cell::Cell::get))
    };
    let (row, count) = searches(&|| document.next_row_with_diagnostic(2));
    assert_eq!(row, Some(last));
    assert!(count <= 3, "{count} searches of the marks for Next");
    let (row, count) = searches(&|| document.previous_row_with_diagnostic(last));
    assert_eq!(row, Some(1));
    assert!(count <= 3, "{count} searches of the marks for Previous");
}

/// ADR-0004 decision 8 (§7): nothing may follow an open unterminated
/// quote's row; inserting before it, or deleting it, is fine.
#[test]
fn nothing_goes_after_an_open_unterminated_quote() {
    let dir = Dir::new("rows-quote");
    let document = open_with(&dir, "a.csv", b"a,b\nc,\"open\nquote", false);
    assert_eq!(document.row_count(), 2);
    assert!(matches!(
        document.insert_rows(2, &rows_of(&[&["x"]])),
        Err(EditError::AfterUnterminatedQuote { row: 2, .. })
    ));
    assert!(document.can_insert_rows(2).is_err());
    assert!(document.can_insert_rows(1).is_ok());
    let before = insert(&document, 1, &[&["x"]]);
    // Closing the quote (editing it) lifts the rule; opening it again
    // while a row follows it is refused.
    let closed = set_cell(&document, 2, 1, "closed");
    let after = insert(&document, 3, &[&["after"]]);
    assert!(matches!(
        document.apply(&closed.inverse()),
        Err(EditError::AfterUnterminatedQuote { .. })
    ));
    document.apply(&after.inverse()).unwrap();
    document.apply(&closed.inverse()).unwrap();
    // Deleting the quote's row lifts it too, and undoing that is fine.
    let gone = delete(&document, 2, 1);
    insert(&document, 2, &[&["end"]]);
    assert!(matches!(
        document.apply(&gone.inverse()),
        Err(EditError::AfterUnterminatedQuote { .. })
    ));
    document.apply(&before.inverse()).unwrap();
    assert_eq!(texts(&document), [vec!["a", "b"], vec!["end"]]);
}

/// ADR-0014 decision 1: no row insert or delete until the whole file has
/// been read.
#[test]
fn rows_cant_change_until_the_file_is_read() {
    let dir = Dir::new("rows-reading");
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
        document.insert_rows(1, &rows_of(&[&["x"]])),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.delete_rows(1, 1),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.can_change_rows(),
        Err(EditError::StillReading)
    ));
    // Cell edits carry on.
    set_cell(&document, 3, 1, "early");
    gate.open();
    wait_for_index(&document);
    assert!(document.can_change_rows().is_ok());
    delete(&document, 1, 1);
    assert_eq!(document.full_value(2, 1).unwrap().as_deref(), Some("early"));
}

/// Find catches up across inserts and deletes, and their undo, without
/// starting again (§5): its answers are a fresh search's.
#[test]
fn find_catches_up_across_inserts_and_deletes() {
    let dir = Dir::new("rows-find");
    let bytes = sample(300 * 1024);
    let document = open_with(&dir, "a.csv", &bytes, true);
    let rows = document.row_count();
    let early = search(&document, "caf");
    let running = document.find(&Query::new("caf")).unwrap();
    let mut done = vec![
        insert(&document, 5, &[&["caf", "x caf"], &["none"]]),
        delete(&document, 10, 3),
    ];
    done.push(insert(&document, rows / 2, &[&["café"]]));
    // A big delete: more rows than a query recounts itself.
    done.push(delete(&document, 100, 500));
    done.push(set_cell(&document, 6, 0, "caf here"));
    done.push(insert(&document, 0, &[&["caf header"]]));
    assert_eq!(running.job().control().wait_timeout(LONG), Some(Ok(())));
    let check = |label: &str| {
        let fresh = search(&document, "caf");
        let want = answers(&fresh);
        for found in [&early, &running] {
            while found.progress().catching_up {
                if let Some(job) = found.catch_up_job() {
                    let _ = job.control().wait_timeout(LONG);
                }
            }
            assert_eq!(answers(found), want, "{label}");
        }
        let window = fresh.matches_in(0..40, 0..3, 10).unwrap();
        assert_eq!(
            early.matches_in(0..40, 0..3, 10).unwrap(),
            window,
            "{label}"
        );
        want.0
    };
    let edited = check("edited");
    for command in done.iter().rev() {
        document.apply(&command.inverse()).unwrap();
    }
    let undone = check("undone");
    assert_ne!(edited, undone);
    assert!(!document.has_edits());
}

/// Replay (and any command from other edits) applies a row command by
/// value: it inserts rows that read the same, and deletes rows only if
/// they read as recorded.
#[test]
fn a_row_command_from_other_edits_applies_by_value() {
    let dir = Dir::new("rows-replay");
    let document = open_with(&dir, "a.csv", FILE, false);
    let history = vec![
        set_cell(&document, 1, 1, "one"),
        insert(&document, 1, &[&["new", "row"]]),
        delete(&document, 2, 2),
    ];
    let fresh = open_with(&dir, "b.csv", FILE, false);
    let replay = fresh.replay(&history);
    assert!(replay.refused.is_empty(), "{:?}", replay.refused);
    assert_eq!(texts(&fresh), texts(&document));
    for command in replay.commands.iter().rev() {
        fresh.apply(&command.inverse()).unwrap();
    }
    assert!(!fresh.has_edits());
    // Without the cell edit, the deleted row reads otherwise: refused.
    let other = open_with(&dir, "c.csv", FILE, false);
    let replay = other.replay(&history[1..]);
    assert_eq!(replay.commands.len(), 1);
    assert!(matches!(
        replay.refused[..],
        [(1, EditError::ValueChanged { row: 2, column: 1 })]
    ));
    // Another lineage is refused by `apply`.
    assert!(matches!(
        other.apply(&history[1]),
        Err(EditError::OtherLineage)
    ));
}
