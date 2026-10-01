//! Find, Copy and the cell inspector (task 1.8).

// A highlight's ranges are a list of `Range`s, often of one.
#![allow(clippy::single_range_in_vec_init)]

use super::*;

use crate::find::{Matcher, Query};

fn wait_for_search(search: &Search) -> SearchSummary {
    assert_eq!(search.job().control().wait_timeout(LONG), Some(Ok(())));
    *search.job().wait().unwrap()
}

/// Every matching cell of `bytes`, by brute force: every row of the whole
/// file, every field's display value, after the header row if any.
fn expected_matches(bytes: &[u8], parser: RowParser, header: bool, query: &Query) -> Vec<Place> {
    let matcher = Matcher::new(query, parser.encoding()).unwrap();
    let index = RowIndex::build(bytes, parser.dialect()).unwrap();
    let mut places = Vec::new();
    for row in usize::from(header)..index.row_count() {
        let parsed = parser.parse_row(&index, row, bytes).unwrap();
        for (column, field) in parsed.fields().iter().enumerate() {
            if matcher.is_match(&parser.display_value(bytes, field)) {
                places.push(Place { row, column });
            }
        }
    }
    places
}

/// Every match a search gives, by stepping forward from the start until it
/// wraps, checking each one's number on the way.
fn walk_matches(search: &Search) -> Vec<Place> {
    let mut places = Vec::new();
    let mut from = None;
    loop {
        match search.step(from, true).unwrap() {
            SearchStep::Found {
                place,
                ordinal,
                wrapped,
            } => {
                if wrapped {
                    assert_eq!(ordinal, 1);
                    break;
                }
                assert_eq!(ordinal, places.len() as u64 + 1, "at {place:?}");
                assert_eq!(search.ordinal(place).unwrap(), Some(ordinal));
                places.push(place);
                from = Some(place);
            }
            SearchStep::NotFound => break,
            SearchStep::Pending => panic!("a complete search is never pending"),
        }
    }
    places
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

const ORDERS: &[u8] = b"order,customer,email\n\
1,Marlow Foods,orders@marlow.example\n\
2,Ostrava Tools,orders@ostrava.example\n\
3,\"Marlow & Daughters\",x@y\n\
4,\"say \"\"marlow\"\"\",\"two\nlines marlow\"\n\
5,marlowe,MARLOW\n";

#[test]
fn find_counts_matching_cells_ignoring_case() {
    let dir = Dir::new("find-count");
    let (document, _) = open_bytes(&dir, "orders.csv", ORDERS);
    wait_for_index(&document);
    let search = document.find(&Query::new("marlow")).unwrap();
    let summary = wait_for_search(&search);
    // Rows 1, 3, 4 and 5 (the header row isn't searched): two cells each
    // in rows 1, 4 and 5, one in row 3.
    assert_eq!(
        summary,
        SearchSummary {
            matches: 7,
            rows: 4
        }
    );
    let progress = search.progress();
    assert!(progress.complete);
    assert_eq!(progress.matches, 7);
    assert_eq!(progress.rows_searched, 6);
    assert_eq!(progress.generation, document.generation());
    let places = walk_matches(&search);
    assert_eq!(
        places,
        [
            at(1, 1),
            at(1, 2),
            at(3, 1),
            at(4, 1),
            at(4, 2),
            at(5, 1),
            at(5, 2)
        ]
    );
    let parser = document.current().parser;
    assert_eq!(
        places,
        expected_matches(ORDERS, parser, true, &Query::new("marlow"))
    );
    // Case-sensitive: only the lower-case ones.
    let exact = document
        .find(&Query {
            text: "marlow".to_owned(),
            case_sensitive: true,
        })
        .unwrap();
    assert_eq!(wait_for_search(&exact).matches, 4);
    // A quote in the query matches the quote of a display value (`""` in
    // the file).
    let quoted = document.find(&Query::new("\"marlow\"")).unwrap();
    assert_eq!(wait_for_search(&quoted).matches, 1);
    assert_eq!(walk_matches(&quoted), [at(4, 1)]);
    // No matches.
    let none = document.find(&Query::new("zebra")).unwrap();
    assert_eq!(wait_for_search(&none).matches, 0);
    assert_eq!(none.step(None, true).unwrap(), SearchStep::NotFound);
    assert_eq!(
        none.step(Some(at(2, 0)), false).unwrap(),
        SearchStep::NotFound
    );
    assert!(document.find(&Query::new("")).is_err());
}

#[test]
fn find_highlights_say_where_in_the_shown_text() {
    let dir = Dir::new("find-ranges");
    let (document, _) = open_bytes(&dir, "orders.csv", ORDERS);
    wait_for_index(&document);
    let search = document.find(&Query::new("marlow")).unwrap();
    wait_for_search(&search);
    // Rows 1 to 3, columns 1 and 2 only.
    let matches = search.matches_in(1..4, 1..3, 100).unwrap();
    let cell = |row, column, ranges: &[Range<usize>]| CellMatch {
        row,
        column,
        ranges: ranges.to_vec(),
    };
    assert_eq!(
        matches,
        [
            cell(1, 1, &[0..6]),
            cell(1, 2, &[7..13]),
            cell(3, 1, &[0..6])
        ]
    );
    // The grid shows the first 12 characters of "two\nlines marlow": the
    // match is cut there. A match wholly past them has no range, but the
    // cell still matches.
    let row4 = search.matches_in(4..5, 0..3, 12).unwrap();
    assert_eq!(row4[1], cell(4, 2, &[10..12]));
    let row4 = search.matches_in(4..5, 2..3, 5).unwrap();
    assert_eq!(row4, [cell(4, 2, &[])]);
    assert_eq!(search.matches_in(2..3, 0..3, 100).unwrap(), []);
}

#[test]
fn next_and_previous_wrap_at_the_edges() {
    let dir = Dir::new("find-edges");
    let (document, _) = open_bytes(&dir, "orders.csv", ORDERS);
    wait_for_index(&document);
    let search = document.find(&Query::new("marlow")).unwrap();
    wait_for_search(&search);
    // From nowhere: the first and the last.
    assert_eq!(search.step(None, true).unwrap(), found(1, 1, 1, false));
    assert_eq!(search.step(None, false).unwrap(), found(5, 2, 7, false));
    // Within a row, then on to the next matching row, and back.
    assert_eq!(
        search.step(Some(at(1, 1)), true).unwrap(),
        found(1, 2, 2, false)
    );
    assert_eq!(
        search.step(Some(at(1, 2)), true).unwrap(),
        found(3, 1, 3, false)
    );
    assert_eq!(
        search.step(Some(at(3, 1)), false).unwrap(),
        found(1, 2, 2, false)
    );
    // From cells that aren't matches.
    assert_eq!(
        search.step(Some(at(2, 2)), true).unwrap(),
        found(3, 1, 3, false)
    );
    assert_eq!(
        search.step(Some(at(2, 0)), false).unwrap(),
        found(1, 2, 2, false)
    );
    assert_eq!(
        search.step(Some(at(4, 0)), true).unwrap(),
        found(4, 1, 4, false)
    );
    assert_eq!(
        search.step(Some(at(4, 2)), false).unwrap(),
        found(4, 1, 4, false)
    );
    // Past the last, and before the first: round the end.
    assert_eq!(
        search.step(Some(at(5, 2)), true).unwrap(),
        found(1, 1, 1, true)
    );
    assert_eq!(
        search.step(Some(at(1, 1)), false).unwrap(),
        found(5, 2, 7, true)
    );
    assert_eq!(
        search.step(Some(at(0, 2)), false).unwrap(),
        found(5, 2, 7, true)
    );
    assert_eq!(
        search.step(Some(at(9, 0)), true).unwrap(),
        found(1, 1, 1, true)
    );
    // Only the matches have numbers.
    assert_eq!(search.ordinal(at(2, 1)).unwrap(), None);
    assert_eq!(search.ordinal(at(1, 0)).unwrap(), None);
    assert_eq!(search.ordinal(at(5, 1)).unwrap(), Some(6));
}

#[test]
fn a_single_match_wraps_to_itself() {
    let dir = Dir::new("find-one");
    let (document, _) = open_bytes(&dir, "orders.csv", ORDERS);
    wait_for_index(&document);
    let search = document.find(&Query::new("Ostrava Tools")).unwrap();
    assert_eq!(wait_for_search(&search).matches, 1);
    assert_eq!(search.step(None, true).unwrap(), found(2, 1, 1, false));
    assert_eq!(
        search.step(Some(at(2, 1)), true).unwrap(),
        found(2, 1, 1, true)
    );
    assert_eq!(
        search.step(Some(at(2, 1)), false).unwrap(),
        found(2, 1, 1, true)
    );
}

#[test]
fn find_agrees_with_a_brute_force_scan_over_many_chunks() {
    let dir = Dir::new("find-brute");
    // About eight chunks, with quoted fields, `""` and newlines in them: in
    // UTF-8, where the raw bytes are searched first, and in Windows-1252
    // (é as one byte), where every value is checked.
    let utf8 = sample(8 * SEARCH_CHUNK_BYTES);
    // Every character is ASCII or é, so its code point is its byte.
    let latin: Vec<u8> = std::str::from_utf8(&utf8)
        .unwrap()
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap())
        .collect();
    for (name, bytes, encoding) in [
        ("utf8.csv", &utf8, Encoding::Utf8),
        ("latin.csv", &latin, Encoding::Windows1252),
    ] {
        let (document, screen) = open_bytes(&dir, name, bytes);
        assert_eq!(screen.detection.encoding, encoding);
        wait_for_index(&document);
        let parser = document.current().parser;
        for text in ["n4", "CAFÉ 9", "\"b\"", "a, \"", "lines", ",", "é"] {
            let query = Query::new(text);
            let search = document.find(&query).unwrap();
            let summary = wait_for_search(&search);
            let expected = expected_matches(bytes, parser, true, &query);
            assert_eq!(summary.matches, expected.len() as u64, "{name} {text:?}");
            assert!(summary.matches > 0, "{name} {text:?}");
            if expected.len() < 5_000 {
                assert_eq!(walk_matches(&search), expected, "{name} {text:?}");
            } else {
                // Spot checks: the middle one and the last.
                let middle = expected[expected.len() / 2];
                assert_eq!(
                    search.ordinal(middle).unwrap(),
                    Some(expected.len() as u64 / 2 + 1)
                );
                let last = *expected.last().unwrap();
                assert_eq!(
                    search.step(None, false).unwrap(),
                    SearchStep::Found {
                        place: last,
                        ordinal: expected.len() as u64,
                        wrapped: false
                    }
                );
            }
        }
    }
}

#[test]
fn find_works_in_a_single_byte_encoding() {
    let dir = Dir::new("find-1252");
    // "café" and "CAFÉ" in Windows-1252: é is 0xE9, É is 0xC9.
    let bytes = b"name,note\ncaf\xe9,x\nCAF\xc9,caf\xe9 caf\xe9\nother,y\n";
    let path = dir.file("latin.csv", bytes);
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler(),
        options(10),
        None,
    )
    .unwrap();
    assert_eq!(screen.detection.encoding, Encoding::Windows1252);
    wait_for_index(&document);
    let search = document.find(&Query::new("café")).unwrap();
    assert_eq!(wait_for_search(&search).matches, 3);
    assert_eq!(
        search.matches_in(2..3, 0..2, 100).unwrap()[1].ranges,
        [0..4, 5..9]
    );
}

#[test]
fn find_runs_while_indexing_and_keeps_up() {
    let dir = Dir::new("find-indexing");
    let bytes = sample(4 << 20);
    let path = dir.file("big.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    // The index is held: the search can't get anywhere yet, and says so.
    let query = Query::new("n6");
    let search = document.find(&query).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let progress = search.progress();
    assert!(!progress.complete);
    assert_eq!(progress.matches, 0);
    assert_eq!(progress.rows_searched, 1);
    assert_eq!(search.step(None, true).unwrap(), SearchStep::Pending);
    assert_eq!(search.step(None, false).unwrap(), SearchStep::Pending);
    assert!(!search.job().is_finished());
    // Once the index runs, the search follows it to the end.
    gate.open();
    let summary = wait_for_search(&search);
    assert!(document.progress().complete);
    let parser = document.current().parser;
    let expected = expected_matches(&bytes, parser, true, &query);
    assert_eq!(summary.matches, expected.len() as u64);
    assert_eq!(search.progress().rows_searched, document.row_count());
}

#[test]
fn a_search_pauses_while_the_user_interacts() {
    let dir = Dir::new("find-pause");
    let bytes = sample(4 << 20);
    let (document, _) = open_bytes(&dir, "big.csv", &bytes);
    wait_for_index(&document);
    let scheduler = document.scheduler.clone();
    scheduler.set_interacting(true);
    let search = document.find(&Query::new("n6")).unwrap();
    assert_eq!(
        search
            .job()
            .control()
            .wait_timeout(Duration::from_millis(300)),
        None
    );
    let paused = search.progress().rows_searched;
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(search.progress().rows_searched, paused);
    scheduler.set_interacting(false);
    wait_for_search(&search);
    assert!(search.progress().complete);
}

#[test]
fn cancelling_a_search_or_closing_the_document_stops_it() {
    let dir = Dir::new("find-cancel");
    let bytes = sample(4 << 20);
    let path = dir.file("big.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let open = || {
        Document::open(
            &path,
            &dir.temp(),
            VolumeInfo::default(),
            &scheduler,
            options(5),
            None,
        )
        .unwrap()
        .0
    };
    let document = open();
    let search = document.find(&Query::new("n6")).unwrap();
    search.cancel();
    assert_eq!(
        search.job().control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    // Closing the document stops the index, and so the search waiting for
    // it.
    let search = document.find(&Query::new("n6")).unwrap();
    drop(document);
    gate.open();
    assert_eq!(
        search.job().control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
    // Dropping the search cancels it.
    let document = open();
    let search = document.find(&Query::new("n6")).unwrap();
    let job = search.job().clone();
    drop(search);
    assert_eq!(
        job.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
}

/// The text a copy job gives.
fn copied(document: &Document, rows: Range<usize>, columns: Range<usize>) -> String {
    let job = document.copy_cells(rows, columns);
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    job.wait().unwrap().take().unwrap()
}

#[test]
fn copying_gives_tab_separated_display_values() {
    let dir = Dir::new("copy");
    // The header's é (UTF-8) keeps the file UTF-8 despite the invalid byte.
    let bytes = "a,b,c\u{e9}\u{e9}\n"
        .bytes()
        .chain(
            *b"\
plain,\"tab\there\",\"two\nlines\"\n\
\"say \"\"hi\"\"\",caf\xff,\"a\"b\n\
short\n\
x,y,z\n",
        )
        .collect::<Vec<u8>>();
    let (document, _) = open_bytes(&dir, "copy.csv", &bytes);
    wait_for_index(&document);
    // Tabs, line breaks and quotes are quoted; the invalid byte is the
    // U+FFFD the grid shows; text after a closing quote reads as it is.
    assert_eq!(
        copied(&document, 1..3, 0..3),
        "plain\t\"tab\there\"\t\"two\nlines\"\n\
         \"say \"\"hi\"\"\"\tcaf\u{FFFD}\t\"\"\"a\"\"b\""
    );
    // One cell: no quotes needed, no line ending.
    assert_eq!(copied(&document, 4..5, 1..2), "y");
    // A short row's missing cells are empty; rows past the end are left
    // out.
    assert_eq!(copied(&document, 3..9, 0..3), "short\t\t\nx\ty\tz");
    // The header row is a row like any other here (the app decides).
    assert_eq!(copied(&document, 0..1, 1..3), "b\tc\u{e9}\u{e9}");
    // Taken once.
    let job = document.copy_cells(4..5, 0..1);
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(job.wait().unwrap().take().as_deref(), Some("x"));
    assert_eq!(job.wait().unwrap().take(), None);
}

#[test]
fn copying_past_the_indexed_rows_waits_for_them() {
    let dir = Dir::new("copy-wait");
    let bytes = sample(400 * 1024);
    let path = dir.file("big.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, screen) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    let parser = document.current().parser;
    let all = expected_rows(&bytes, parser, usize::MAX);
    // Select All: every row the estimate says there are, and more.
    let job = document.copy_cells(1..screen.estimated_row_count * 2, 0..3);
    assert_eq!(job.control().wait_timeout(Duration::from_millis(100)), None);
    gate.open();
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    let text = job.wait().unwrap().take().unwrap();
    let mut expected = String::new();
    for (i, row) in all[1..].iter().enumerate() {
        if i > 0 {
            expected.push('\n');
        }
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                expected.push('\t');
            }
            push_tsv_cell(&mut expected, &cell.text);
        }
    }
    assert_eq!(text, expected);
}

#[test]
fn the_inspector_gets_whole_values() {
    let dir = Dir::new("inspect");
    let long = "x".repeat(300_000);
    // The header's é (UTF-8) keeps the file UTF-8 despite the invalid byte.
    let mut bytes = "a,b\u{e9}\u{e9}\n".as_bytes().to_vec();
    bytes.extend_from_slice(
        b"\"Deliver to the rear entrance.\r\nCall on arrival.\nGate code 4471\",caf\xff\n",
    );
    bytes.extend_from_slice(format!("{long},\n").as_bytes());
    bytes.extend_from_slice(b"short\n");
    let (document, _) = open_bytes(&dir, "inspect.csv", &bytes);
    wait_for_index(&document);
    let notes = document.cell_value(1, 0, 1_000).unwrap().unwrap();
    assert_eq!(
        notes,
        CellValue {
            text: "Deliver to the rear entrance.\r\nCall on arrival.\nGate code 4471".to_owned(),
            truncated: false,
            characters: 62,
            lines: 3,
            invalid: false,
            exists: true,
        }
    );
    let invalid = document.cell_value(1, 1, 1_000).unwrap().unwrap();
    assert_eq!(invalid.text, "caf\u{FFFD}");
    assert!(invalid.invalid);
    assert_eq!((invalid.characters, invalid.lines), (4, 1));
    // A very long value: its start, and its whole length.
    let long_value = document.cell_value(2, 0, 1_000).unwrap().unwrap();
    assert_eq!(long_value.text, long[..1_000]);
    assert!(long_value.truncated);
    assert_eq!((long_value.characters, long_value.lines), (300_000, 1));
    let empty = document.cell_value(2, 1, 1_000).unwrap().unwrap();
    assert_eq!(
        (empty.text.as_str(), empty.lines, empty.exists),
        ("", 0, true)
    );
    // A short row's missing cell, and a row that isn't there.
    assert!(!document.cell_value(3, 1, 1_000).unwrap().unwrap().exists);
    assert_eq!(document.cell_value(9, 0, 1_000).unwrap(), None);
}

/// The race the 1.8 review found: a step looks at the matches, the last
/// chunk lands and the search finishes, then the step looks at whether the
/// search is complete. It must look at the new matches, not wrap round to
/// the first (Next) or call the previous one a wrap (Previous).
#[test]
fn a_step_that_races_the_last_chunk_looks_at_it() {
    let dir = Dir::new("find-race");
    let path = dir.file("race.csv", b"a,b\n1,marlow\n2,x\n3,marlow\n4,y\n");
    // A search over a document whose index is held: it finds nothing until
    // the hook lets the index (and so the search) run to the end.
    let held_search = || {
        let gate = Gate::closed();
        let scheduler = scheduler_with(Arc::clone(&gate));
        let (document, _) = Document::open(
            &path,
            &dir.temp(),
            VolumeInfo::default(),
            &scheduler,
            options(5),
            None,
        )
        .unwrap();
        let search = document.find(&Query::new("marlow")).unwrap();
        assert_eq!(search.progress().matches, 0);
        let job = search.job().clone();
        search.before_settling(move || {
            gate.open();
            assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
        });
        (document, search)
    };

    let (_document, search) = held_search();
    assert_eq!(
        search.step(Some(at(2, 0)), true).unwrap(),
        found(3, 1, 2, false)
    );

    let (_document, search) = held_search();
    assert_eq!(
        search.step(Some(at(4, 0)), false).unwrap(),
        found(3, 1, 2, false)
    );
}

/// Copy at once (task 1.8 review): the same text as the job, for rows the
/// index has; `None` for rows it hasn't reached yet.
#[test]
fn copying_at_once_gives_the_jobs_text_for_indexed_rows() {
    let dir = Dir::new("copy-now");
    let bytes = sample(200 * 1024);
    let path = dir.file("big.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    // The index is held: nothing can be copied at once yet.
    assert_eq!(document.copy_cells_now(1..3, 0..3).unwrap(), None);
    gate.open();
    wait_for_index(&document);
    let rows = document.row_count();
    for (selection, columns) in [(1..3, 0..3), (5..900, 1..3), (rows - 2..rows + 50, 0..2)] {
        assert_eq!(
            document
                .copy_cells_now(selection.clone(), columns.clone())
                .unwrap()
                .as_deref(),
            Some(copied(&document, selection, columns).as_str())
        );
    }
    assert_eq!(
        document
            .copy_cells_now(rows..rows + 5, 0..3)
            .unwrap()
            .as_deref(),
        Some("")
    );
}

/// The estimate the app asks about before a very large copy.
#[test]
fn a_copys_size_is_estimated_without_reading_rows() {
    let dir = Dir::new("copy-estimate");
    let bytes = sample(2 << 20);
    let path = dir.file("big.csv", &bytes);
    let gate = Gate::closed();
    let scheduler = scheduler_with(Arc::clone(&gate));
    let (document, _) = Document::open(
        &path,
        &dir.temp(),
        VolumeInfo::default(),
        &scheduler,
        options(5),
        None,
    )
    .unwrap();
    // Nothing indexed: every row to the end is the rest of the file.
    let len = bytes.len() as u64;
    assert_eq!(document.estimated_copy_bytes(1..usize::MAX, 0..3), len);
    gate.open();
    wait_for_index(&document);
    let rows = document.row_count();
    let all = copied(&document, 1..rows, 0..3).len() as u64;
    let estimate = document.estimated_copy_bytes(1..usize::MAX, 0..3);
    assert!(estimate.abs_diff(all) * 10 < all, "{estimate} vs {all}");
    // One column of three: about a third.
    let one = document.estimated_copy_bytes(1..rows, 2..3);
    assert!(
        one.abs_diff(estimate / 3) * 10 < estimate / 3,
        "{one} vs {estimate}"
    );
    assert_eq!(document.estimated_copy_bytes(5..5, 0..3), 0);
}

/// A copy promised to the pasteboard finishes with exactly the cells it was
/// asked for, even if the document closes (which stops its index) or the
/// file is read again while it waits for rows (task 1.8 re-review).
#[test]
fn a_copy_outlives_its_document_and_a_new_reading() {
    let dir = Dir::new("copy-outlives");
    let bytes = sample(400 * 1024);
    let path = dir.file("big.csv", &bytes);
    let expected = {
        let (document, _) = open_bytes(&dir, "plain.csv", &bytes);
        wait_for_index(&document);
        copied(&document, 1..usize::MAX, 0..3)
    };
    let held = || {
        let gate = Gate::closed();
        let scheduler = scheduler_with(Arc::clone(&gate));
        let (document, _) = Document::open(
            &path,
            &dir.temp(),
            VolumeInfo::default(),
            &scheduler,
            options(5),
            None,
        )
        .unwrap();
        // ⌘A ⌘C while the index is held.
        let job = document.copy_cells(1..usize::MAX, 0..3);
        assert_eq!(job.control().wait_timeout(Duration::from_millis(50)), None);
        (document, gate, job)
    };

    // The document closes: its index is cancelled before it reaches the
    // rows, so the copy indexes the file itself.
    let (document, gate, job) = held();
    drop(document);
    gate.open();
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(
        job.wait().unwrap().take().as_deref(),
        Some(expected.as_str())
    );

    // The file is read again, as semicolons: the copy is of the reading it
    // started in.
    let (document, gate, job) = held();
    document
        .reinterpret(
            Choices {
                delimiter: Some(Delimiter::Semicolon),
                ..Choices::default()
            },
            5,
            100,
        )
        .unwrap();
    gate.open();
    assert_eq!(job.control().wait_timeout(LONG), Some(Ok(())));
    assert_eq!(
        job.wait().unwrap().take().as_deref(),
        Some(expected.as_str())
    );

    // Cancelling it still stops it.
    let (_document, gate, job) = held();
    job.cancel();
    gate.open();
    assert_eq!(
        job.control().wait_timeout(LONG),
        Some(Err(JobError::Cancelled))
    );
}
