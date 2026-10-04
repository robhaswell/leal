//! Paste and Clear (task 2.6): one command each, undone and redone and
//! replayed like any cell edit; the hatched-cell rule (ADR-0005 decision
//! 2); the refusals; Find catching up; and the saved bytes, where only the
//! cells pasted or cleared change.

use super::*;

use leal_testkit::fidelity::assert_identical;

use crate::dialect::{Encoding, LineEnding};
use crate::edit::{CELL_BATCH_LIMIT, Command, Edit, EditError, PASTE_BYTE_LIMIT};
use crate::find::Query;
use crate::save::{SaveError, SaveKind, SaveRequest};

/// A header row, rows of three cells, a short row and a blank line.
const FILE: &[u8] = b"id,name,city\n1,Marlow,Leeds\n2,Ostrava\n\n3,Halden,York\n";

fn open_with(dir: &Dir, name: &str, bytes: &[u8]) -> Arc<Document> {
    let path = dir.file(name, bytes);
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        None,
    )
    .unwrap();
    wait_for_index(&document);
    Arc::new(document)
}

fn texts(document: &Document) -> Vec<Vec<String>> {
    document
        .rows(0..document.row_count(), 1000)
        .unwrap()
        .into_iter()
        .map(|row| row.into_iter().map(|cell| cell.text).collect())
        .collect()
}

fn of(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| row.iter().map(|&value| value.to_owned()).collect())
        .collect()
}

/// Saves `document` over its own file and returns the bytes written.
fn saved(document: &Arc<Document>) -> Vec<u8> {
    let path = document.original().path.clone();
    let job = document.save(SaveRequest::new(&path, SaveKind::Save));
    job.wait().as_ref().unwrap();
    std::fs::read(&path).unwrap()
}

/// Pastes `text` into an LF file's selection, three columns shown.
fn try_paste(
    document: &Document,
    rows: Range<usize>,
    columns: Range<usize>,
    text: &str,
) -> Result<Option<Command>, EditError> {
    document
        .paste(rows, columns, 3, text, LineEnding::Lf)
        .map(|pasting| pasting.command)
}

fn paste(document: &Document, rows: Range<usize>, columns: Range<usize>, text: &str) -> Command {
    try_paste(document, rows, columns, text)
        .unwrap()
        .expect("a change")
}

/// A block goes in from the selection's top-left cell, as one command of
/// its cells; its undo puts every cell back, its redo pastes again.
#[test]
fn a_block_pastes_from_the_top_left_cell_as_one_command() {
    let dir = Dir::new("paste-block");
    let document = open_with(&dir, "a.csv", FILE);
    let before = texts(&document);
    // Selected: rows 1 to 4, columns 1 and 2; the block is 2 by 2.
    let command = paste(&document, 1..5, 1..3, "A\tB\r\nC\tD\r\n");
    let Edit::SetCells(changes) = &command.edit else {
        panic!("one batch: {command:?}");
    };
    assert_eq!(changes.len(), 4);
    assert_eq!(
        texts(&document),
        of(&[
            &["id", "name", "city"],
            &["1", "A", "B"],
            &["2", "C", "D"],
            &[""],
            &["3", "Halden", "York"],
        ])
    );
    document.apply(&command.inverse()).unwrap();
    assert_eq!(texts(&document), before);
    assert!(!document.has_edits());
    document.apply(&command).unwrap();
    assert_eq!(texts(&document)[2], ["2", "C", "D"]);
}

/// One value goes into every selected cell. A hatched cell (past a short
/// row's end) takes it as typing would, padding the row; `""` there is no
/// edit (ADR-0005 decision 2).
#[test]
fn one_value_fills_the_selection_and_follows_the_hatched_cell_rule() {
    let dir = Dir::new("paste-fill");
    let document = open_with(&dir, "a.csv", FILE);
    paste(&document, 2..4, 1..3, "x\n");
    assert_eq!(texts(&document)[2], ["2", "x", "x"]);
    assert_eq!(texts(&document)[3], ["", "x", "x"], "the blank line padded");
    // Undone: the short row and the blank line are as they were.
    let command = paste(&document, 1..2, 0..1, "only this");
    assert_eq!(texts(&document)[1], ["only this", "Marlow", "Leeds"]);
    document.apply(&command.inverse()).unwrap();

    let fresh = open_with(&dir, "b.csv", FILE);
    assert!(
        try_paste(&fresh, 2..4, 2..3, "\n").unwrap().is_none(),
        "\"\" into hatched cells is no edit"
    );
    assert!(!fresh.has_edits());
}

/// Empty clipboard text pastes nothing (Rob, 2026-10-04): it doesn't
/// clear the selection. A lone line break is one empty value (a
/// spreadsheet's copy of an empty cell), which does.
#[test]
fn empty_text_pastes_nothing() {
    let dir = Dir::new("paste-empty");
    let document = open_with(&dir, "a.csv", FILE);
    let pasting = document.paste(1..2, 0..3, 3, "", LineEnding::Lf).unwrap();
    assert!(pasting.command.is_none());
    assert_eq!((pasting.rows, pasting.columns), (0, 0));
    assert!(!document.has_edits());
    paste(&document, 1..2, 0..1, "\r\n");
    assert_eq!(texts(&document)[1], ["", "Marlow", "Leeds"]);
}

/// The paste says the size of what it pasted, for the app to select the
/// block: no second reading of the text.
#[test]
fn a_paste_gives_the_blocks_size() {
    let dir = Dir::new("paste-shape");
    let document = open_with(&dir, "a.csv", FILE);
    let pasting = document
        .paste(1..2, 0..1, 3, "a\tb\tc\nd\n", LineEnding::Lf)
        .unwrap();
    assert!(pasting.command.is_some());
    assert_eq!((pasting.rows, pasting.columns), (2, 3));
    let one = document.paste(1..3, 0..2, 3, "v", LineEnding::Lf).unwrap();
    assert_eq!((one.rows, one.columns), (1, 1));
}

/// Delete clears the selection as one command; a hatched cell stays
/// missing, and one edited there goes back to missing, so the row's bytes
/// come back. Cells already empty make no command.
#[test]
fn clearing_empties_cells_and_leaves_hatched_cells_missing() {
    let dir = Dir::new("clear");
    let document = open_with(&dir, "a.csv", FILE);
    let typed = document.set_cell(2, 2, "typed").unwrap().unwrap();
    assert_eq!(texts(&document)[2], ["2", "Ostrava", "typed"]);
    let command = document.clear_cells(1..4, 1..3).unwrap().unwrap();
    assert_eq!(
        texts(&document)[1..4],
        of(&[&["1", "", ""], &["2", ""], &[""]])
    );
    document.apply(&command.inverse()).unwrap();
    document.apply(&typed.inverse()).unwrap();
    assert!(!document.has_edits());
    assert!(
        document.clear_cells(3..4, 0..3).unwrap().is_none(),
        "a blank line has nothing to clear"
    );
    let saved_bytes = {
        document.clear_cells(2..3, 2..3).unwrap();
        saved(&document)
    };
    assert_identical(FILE, &saved_bytes);
}

/// The refusals, each leaving the document as it was.
#[test]
fn pastes_that_dont_fit_or_are_too_large_are_refused() {
    let dir = Dir::new("paste-refused");
    let document = open_with(&dir, "a.csv", FILE);
    let rows = document.row_count();
    assert!(matches!(
        try_paste(&document, rows - 1..rows, 0..1, "a\nb"),
        Err(EditError::PastLastRow { rows: 2 })
    ));
    assert!(matches!(
        try_paste(&document, 1..2, 2..3, "a\tb"),
        Err(EditError::PastLastColumn { columns: 2 })
    ));
    // One value into a selection wider than the grid's columns.
    assert!(matches!(
        try_paste(&document, 1..2, 2..4, "a"),
        Err(EditError::PastLastColumn { columns: 2 })
    ));
    // The grid's columns, not the row's: a block may reach hatched cells.
    paste(&document, 2..3, 1..2, "a\tb");
    let many = "x\n".repeat(CELL_BATCH_LIMIT + 1);
    assert!(matches!(
        try_paste(&document, 1..2, 0..1, &many),
        Err(EditError::TooManyCells { count }) if count == CELL_BATCH_LIMIT + 1
    ));
    let long = "y".repeat(PASTE_BYTE_LIMIT / 2 + 1);
    assert!(matches!(
        try_paste(&document, 1..3, 0..1, &long),
        Err(EditError::TooMuchText { .. })
    ));
    assert!(matches!(
        try_paste(&document, 1..2, 0..1, &"z".repeat(PASTE_BYTE_LIMIT + 1)),
        Err(EditError::TooMuchText { .. })
    ));
    assert!(matches!(
        document.clear_cells(0..CELL_BATCH_LIMIT, 0..2),
        Err(EditError::TooManyCells { .. })
    ));
    assert!(matches!(
        document.can_clear_cells(0..CELL_BATCH_LIMIT + 1, 0..1),
        Err(EditError::TooManyCells { .. })
    ));
    assert!(matches!(
        document.clear_cells(rows..rows + 1, 0..1),
        Err(EditError::NoSuchRow { .. })
    ));
    assert_eq!(texts(&document)[2], ["2", "a", "b"]);
    assert!(document.can_paste().is_ok());
    assert!(document.can_clear_cells(1..rows, 0..3).is_ok());
}

/// The limit is on what is written: a block under it as it was copied,
/// whose line breaks grow into the file's `\r\n`, is over it and refused,
/// as one value is. The document is unchanged.
#[test]
fn a_block_that_grows_past_the_limit_with_line_breaks_is_refused() {
    let dir = Dir::new("paste-block-grows");
    let document = open_with(&dir, "a.csv", FILE);
    let before = saved(&document);
    let text = format!("\"{}\"\tb", "\r".repeat(PASTE_BYTE_LIMIT - 16));
    assert!(text.len() < PASTE_BYTE_LIMIT);
    assert!(matches!(
        document.paste(1..2, 0..1, 3, &text, LineEnding::Crlf),
        Err(EditError::TooMuchText { bytes }) if bytes > PASTE_BYTE_LIMIT
    ));
    // As it came, it fits.
    assert!(document.paste(1..2, 0..1, 3, &text, LineEnding::Cr).is_ok());
    assert_ne!(saved(&document), before);
}

/// Nothing goes after an unterminated quote (ADR-0004 decision 8): a paste
/// reaching past the quote's cell in its row is refused, whole.
#[test]
fn a_paste_after_an_unterminated_quote_is_refused_whole() {
    let dir = Dir::new("paste-quote");
    let bytes = b"a,b,c\n1,2,3\n4,\"open\n5,6\n";
    let document = open_with(&dir, "a.csv", bytes);
    assert!(matches!(
        try_paste(&document, 1..3, 2..3, "x\ny"),
        Err(EditError::AfterUnterminatedQuote { row: 2, .. })
    ));
    assert!(!document.has_edits(), "none of the block went in");
    // Before it, it can.
    paste(&document, 1..3, 0..1, "p\nq");
    assert_eq!(texts(&document)[1][0], "p");
}

/// Paste and Clear wait for the whole file, like rows and columns
/// (ADR-0014 decision 1), and for a save to end.
#[test]
fn paste_and_clear_wait_for_the_whole_file() {
    let dir = Dir::new("paste-reading");
    let bytes = sample(400 * 1024);
    let path = dir.file("big.csv", &bytes);
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
    assert!(matches!(document.can_paste(), Err(EditError::StillReading)));
    assert!(matches!(
        try_paste(&document, 1..2, 0..1, "x"),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.clear_cells(1..2, 0..1),
        Err(EditError::StillReading)
    ));
    assert!(matches!(
        document.can_clear_cells(1..2, 0..1),
        Err(EditError::StillReading)
    ));
    gate.open();
    wait_for_index(&document);
    assert!(try_paste(&document, 1..2, 0..1, "x").unwrap().is_some());
    assert!(document.clear_cells(1..2, 0..1).unwrap().is_some());
}

/// A search catches up with a paste and with its undo, without starting
/// again (ADR-0014 decision 2, as for cell edits).
#[test]
fn find_catches_up_after_a_paste_and_a_clear() {
    let dir = Dir::new("paste-find");
    let document = open_with(&dir, "a.csv", FILE);
    let search = document.find(&Query::new("marlow")).unwrap();
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(search.progress().matches, 1);
    let command = paste(&document, 2..5, 1..2, "Marlow\nmarlow again\nMARLOW");
    assert_eq!(search.progress().matches, 4);
    let cleared = document.clear_cells(1..3, 1..2).unwrap().unwrap();
    assert_eq!(search.progress().matches, 2);
    document.apply(&cleared.inverse()).unwrap();
    document.apply(&command.inverse()).unwrap();
    assert_eq!(search.progress().matches, 1);
}

/// The journal's replay (ADR-0008 decision 5) applies a paste by value
/// into a fresh document of the same file, which then saves the same
/// bytes.
#[test]
fn a_replayed_paste_saves_the_same_bytes() {
    let dir = Dir::new("paste-replay");
    let document = open_with(&dir, "a.csv", FILE);
    let commands = vec![
        paste(&document, 1..3, 1..3, "new, name\t\"q\"\nline\nbreak\t"),
        document.clear_cells(4..5, 2..3).unwrap().unwrap(),
    ];
    let fresh = open_with(&dir, "b.csv", FILE);
    let replay = fresh.replay(&commands);
    assert!(replay.refused.is_empty(), "{:?}", replay.refused);
    assert_eq!(texts(&fresh), texts(&document));
    assert_eq!(saved(&fresh), saved(&document));
}

/// Byte fidelity (DESIGN §5): only the cells pasted or cleared change on
/// disk. The rest keep their quoting, escaped quotes, CRLF line endings and
/// spaces; values that need quotes (a comma, a quote, a line break) are
/// quoted; a multi-line value keeps its own line breaks.
#[test]
fn only_the_pasted_and_cleared_cells_change_on_disk() {
    let dir = Dir::new("paste-bytes");
    let bytes: &[u8] = b"id,name,note\r\n\
1,\"Marlow\",\"say \"\"hi\"\"\"\r\n\
2, spaced ,\"multi\r\nline\"\r\n\
3,plain,last\r\n";
    let document = open_with(&dir, "a.csv", bytes);
    // A block over row 1's name and note, and row 2's name. The LF in
    // the quoted value becomes the file's CRLF.
    document
        .paste(
            1..3,
            1..3,
            3,
            "a, comma\t\"two\nlines\"\r\nsays \"\"x\"\"\r\n",
            LineEnding::Crlf,
        )
        .unwrap()
        .command
        .expect("a change");
    assert_eq!(
        texts(&document)[2][1],
        "says \"\"x\"\"",
        "not a quoted cell: its text as it is"
    );
    // Row 3's note cleared.
    document.clear_cells(3..4, 2..3).unwrap().unwrap();
    let expected: &[u8] = b"id,name,note\r\n\
1,\"a, comma\",\"two\r\nlines\"\r\n\
2,\"says \"\"\"\"x\"\"\"\"\",\"multi\r\nline\"\r\n\
3,plain,\r\n";
    assert_eq!(
        String::from_utf8_lossy(&saved(&document)),
        String::from_utf8_lossy(expected)
    );

    // A one-value paste over cells already holding it changes nothing, so
    // the file saves as it was.
    let other = open_with(&dir, "b.csv", bytes);
    assert!(
        try_paste(&other, 3..4, 1..2, "plain\r\n")
            .unwrap()
            .is_none()
    );
    paste(&other, 3..4, 1..2, "PLAIN");
    paste(&other, 3..4, 1..2, "plain");
    assert_identical(bytes, &saved(&other));
}

/// A hatched cell pasted into is written at the row's end, with the
/// delimiters needed to reach it; cleared again before a save, the row
/// keeps its bytes.
#[test]
fn a_paste_into_hatched_cells_appends_to_the_row() {
    let dir = Dir::new("paste-hatched");
    let document = open_with(&dir, "a.csv", FILE);
    paste(&document, 2..4, 1..3, "a\tb\nc\td");
    assert_eq!(
        String::from_utf8_lossy(&saved(&document)),
        "id,name,city\n1,Marlow,Leeds\n2,a,b\n,c,d\n3,Halden,York\n"
    );
    let other = open_with(&dir, "b.csv", FILE);
    paste(&other, 2..3, 2..3, "far");
    other.clear_cells(2..3, 1..3).unwrap().unwrap();
    assert_eq!(texts(&other)[2], ["2", ""]);
    let other_bytes = saved(&other);
    assert_eq!(
        String::from_utf8_lossy(&other_bytes),
        "id,name,city\n1,Marlow,Leeds\n2,\n\n3,Halden,York\n"
    );
}

/// A line break inside a pasted value (LF, CRLF or a lone CR on the
/// clipboard) is saved as the file's own line ending, as a typed one is
/// (Rob, 2026-10-04); the other rows keep their bytes.
#[test]
fn line_breaks_in_pasted_values_are_the_files() {
    let dir = Dir::new("paste-line-breaks");
    let text = "\"lf\nx\"\t\"crlf\r\nx\"\t\"cr\rx\"\r\n";
    for (name, file, ending, saved_row) in [
        (
            "crlf.csv",
            &b"a,b,c\r\n1,2,3\r\n4,5,6\r\n"[..],
            LineEnding::Crlf,
            "\"lf\r\nx\",\"crlf\r\nx\",\"cr\r\nx\"\r\n",
        ),
        (
            "lf.csv",
            &b"a,b,c\n1,2,3\n4,5,6\n"[..],
            LineEnding::Lf,
            "\"lf\nx\",\"crlf\nx\",\"cr\nx\"\n",
        ),
    ] {
        let document = open_with(&dir, name, file);
        document
            .paste(1..2, 0..1, 3, text, ending)
            .unwrap()
            .command
            .expect("a change");
        let line = std::str::from_utf8(ending.bytes()).unwrap();
        let expected = format!("a,b,c{line}{saved_row}4,5,6{line}");
        assert_eq!(
            String::from_utf8_lossy(&saved(&document)),
            expected,
            "{name}"
        );
    }
}

/// Paste into a semicolon-separated and a tab-separated file: the pasted
/// cells are written with the file's own delimiter, quoted only where it
/// (or a quote, or a line break) needs it, and every other byte is the
/// file's.
#[test]
fn a_paste_into_semicolon_and_tab_files_writes_their_delimiter() {
    let dir = Dir::new("paste-delimiters");
    let semicolons: &[u8] = b"id;name;note\n1;\"Marlow\";x, y\n2;Ostrava;z\n";
    let document = open_with(&dir, "s.csv", semicolons);
    paste(&document, 1..3, 1..3, "a;b\tc, d\ne\t\"f\tg\"");
    assert_eq!(
        String::from_utf8_lossy(&saved(&document)),
        "id;name;note\n1;\"a;b\";c, d\n2;e;f\tg\n"
    );
    let tabs: &[u8] = b"id\tname\tnote\n1\tMarlow\tx; y\n2\tOstrava\t\"q\"\n";
    let document = open_with(&dir, "t.tsv", tabs);
    paste(&document, 1..3, 1..2, "a,b\n\"c\td\"");
    assert_eq!(
        String::from_utf8_lossy(&saved(&document)),
        "id\tname\tnote\n1\ta,b\tx; y\n2\t\"c\td\"\t\"q\"\n"
    );
}

/// A pasted value Windows-1252 can't hold stops Save, naming the cell,
/// and the file is untouched; Save As UTF-8 writes it (DESIGN §3.7).
#[test]
fn a_pasted_value_the_encoding_cant_hold_is_saved_as_utf8() {
    let dir = Dir::new("paste-1252");
    let bytes: &[u8] = b"caf\xE9,b\r\n1,2\r\n3,4\r\n";
    let document = open_with(&dir, "w.csv", bytes);
    let path = document.original().path.clone();
    assert_eq!(document.detection().encoding, Encoding::Windows1252);
    paste(&document, 1..3, 1..2, "\u{1F600}\nok");
    let job = document.save(SaveRequest::new(&path, SaveKind::Save));
    assert!(
        matches!(job.wait(), Err(SaveError::Unencodable { cells, .. }) if cells == &[(1, 1)]),
        "{:?}",
        job.wait()
    );
    assert_identical(bytes, &std::fs::read(&path).unwrap());
    let copy = dir.0.join("u8.csv");
    let job = document.save(SaveRequest::new(&copy, SaveKind::SaveAsUtf8));
    job.wait().as_ref().unwrap();
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        "café,b\r\n1,\u{1F600}\r\n3,ok\r\n".as_bytes()
    );
}

/// The cells' old values, which a paste's or a clear's command keeps for
/// undo, may come to at most `PASTE_BYTE_LIMIT` bytes: past it the command
/// is refused before anything changes, though it is within the cell limit.
#[test]
fn replacing_too_much_text_at_once_is_refused() {
    let dir = Dir::new("paste-replaced");
    let big = "v".repeat(PASTE_BYTE_LIMIT / 3 + 1);
    let mut bytes = b"a,b\n".to_vec();
    for _ in 0..3 {
        bytes.extend_from_slice(format!("{big},x\n").as_bytes());
    }
    let document = open_with(&dir, "big.csv", &bytes);
    assert!(matches!(
        document.clear_cells(1..4, 0..2),
        Err(EditError::TooMuchReplaced { bytes }) if bytes > PASTE_BYTE_LIMIT
    ));
    assert!(matches!(
        try_paste(&document, 1..4, 0..1, "small"),
        Err(EditError::TooMuchReplaced { .. })
    ));
    assert!(!document.has_edits());
    // Two of them are within it.
    assert!(document.clear_cells(1..3, 0..1).unwrap().is_some());
}
