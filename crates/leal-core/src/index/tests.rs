//! Tests for the row index: hand-written cases, the testkit's generated
//! files (clean and messy, UTF-8 and UTF-16) and every corpus sidecar.
//!
//! The expected answers come from the testkit, which is checked against its
//! own reference parser; that parser is never used here (0.2 notes).

use super::scan::{RowFacts, RowObserver};
use super::*;

use std::sync::atomic::Ordering;

use leal_testkit::corpus::CorpusCase;
use leal_testkit::diagnostics::DiagnosticKind;
use leal_testkit::dialect::{Encoding, LineEnding as TkLineEnding};
use leal_testkit::layout::Layout;
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, GeneratedCsv, csv_file, csv_file_utf16};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers

fn utf8(delimiter: u8) -> IndexDialect {
    IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: CodeUnit::Byte,
        bom_len: 0,
    }
}

/// Collects what the scanner reports for each row.
#[derive(Default)]
struct Rows(Vec<RowFacts>);

impl RowObserver for Rows {
    fn row(&mut self, facts: &RowFacts) {
        self.0.push(facts.clone());
    }
}

/// Indexes on this thread with a given chunk size, collecting each row's
/// facts.
fn index_with(bytes: &[u8], dialect: IndexDialect, chunk: usize) -> (RowIndex, Vec<RowFacts>) {
    let index = RowIndex::new(dialect).unwrap();
    let mut rows = Rows::default();
    fill(
        &index,
        bytes,
        &AtomicBool::new(false),
        chunk,
        |_| {},
        &mut rows,
    )
    .unwrap();
    assert_eq!(index.status(), Status::Complete);
    (index, rows.0)
}

/// Every row's span and line ending.
fn spans(index: &RowIndex, bytes: &[u8]) -> Vec<RowSpan> {
    (0..index.row_count())
        .map(|r| index.row(r, bytes).unwrap())
        .collect()
}

/// `(span, line ending)` for each row of `bytes`, comma-delimited UTF-8.
fn rows_of(bytes: &[u8]) -> Vec<(Range<usize>, Option<LineEnding>)> {
    let index = RowIndex::build(bytes, utf8(b',')).unwrap();
    spans(&index, bytes)
        .into_iter()
        .map(|r| (r.span, r.line_ending))
        .collect()
}

fn field_counts(bytes: &[u8], dialect: IndexDialect) -> Vec<usize> {
    index_with(bytes, dialect, CHUNK_BYTES)
        .1
        .iter()
        .map(|r| r.fields)
        .collect()
}

use LineEnding::{Cr, Crlf, Lf};

fn tk(le: TkLineEnding) -> LineEnding {
    match le {
        TkLineEnding::Lf => Lf,
        TkLineEnding::Crlf => Crlf,
        TkLineEnding::Cr => Cr,
    }
}

fn dialect_for(delimiter: u8, encoding: Encoding, bom_len: usize) -> IndexDialect {
    IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: match encoding {
            Encoding::Utf8 | Encoding::Windows1252 => CodeUnit::Byte,
            Encoding::Utf16Le => CodeUnit::Utf16Le,
            Encoding::Utf16Be => CodeUnit::Utf16Be,
        },
        bom_len,
    }
}

fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let mut bytes = if little_endian {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for unit in text.encode_utf16() {
        let pair = if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&pair);
    }
    bytes
}

fn utf16_dialect(little_endian: bool) -> IndexDialect {
    IndexDialect {
        delimiter: b',',
        quote: b'"',
        code_unit: if little_endian {
            CodeUnit::Utf16Le
        } else {
            CodeUnit::Utf16Be
        },
        bom_len: 2,
    }
}

/// Checks the index of `bytes` against a layout the testkit says a parser
/// must find: every row's span and line ending, every row's field count,
/// the dominant field count and the unterminated quote.
fn check_against_layout(
    bytes: &[u8],
    layout: &Layout,
    dialect: IndexDialect,
    chunk: usize,
) -> Result<(), TestCaseError> {
    let (index, facts) = index_with(bytes, dialect, chunk);
    prop_assert_eq!(index.row_count(), layout.rows.len());
    let expected: Vec<RowSpan> = layout
        .rows
        .iter()
        .map(|r| RowSpan {
            span: r.span.clone(),
            line_ending: r.line_ending.map(tk),
        })
        .collect();
    prop_assert_eq!(spans(&index, bytes), expected);
    let counts: Vec<usize> = facts.iter().map(|r| r.fields).collect();
    prop_assert_eq!(counts, layout.field_counts());
    prop_assert_eq!(index.field_count_mode(), layout.field_count_mode());
    let unterminated = layout
        .rows
        .last()
        .and_then(|r| r.fields.last())
        .filter(|f| f.unterminated)
        .map(|f| f.span.start);
    prop_assert_eq!(index.unterminated_quote(), unterminated);
    // The observer and the stored index agree.
    for (r, f) in facts.iter().enumerate() {
        let row = index.row(r, bytes).unwrap();
        prop_assert_eq!(f.row, r);
        prop_assert_eq!(&f.span, &row.span);
        prop_assert_eq!(f.line_ending, row.line_ending);
        prop_assert_eq!(f.next_start, index.row_extent(r).unwrap().end);
    }
    Ok(())
}

fn check_generated(file: &GeneratedCsv, chunk: usize) -> Result<(), TestCaseError> {
    let dialect = dialect_for(file.delimiter().byte(), file.encoding, file.layout.bom_len);
    check_against_layout(&file.bytes, &file.layout, dialect, chunk)
}

// ---------------------------------------------------------------------------
// Rows: hand-written cases

#[test]
fn an_empty_file_has_no_rows() {
    let index = RowIndex::build(b"", utf8(b',')).unwrap();
    assert_eq!(index.status(), Status::Complete);
    assert_eq!(index.row_count(), 0);
    assert_eq!(index.row(0, b""), None);
    assert_eq!(index.field_count_mode(), None);
    assert_eq!(index.unterminated_quote(), None);
}

#[test]
fn a_bom_only_file_has_no_rows() {
    let bytes = b"\xEF\xBB\xBF";
    let dialect = IndexDialect {
        bom_len: 3,
        ..utf8(b',')
    };
    let index = RowIndex::build(bytes, dialect).unwrap();
    assert_eq!(index.row_count(), 0);
}

#[test]
fn row_zero_starts_after_the_bom() {
    let bytes = b"\xEF\xBB\xBF\"a\nb\",c\nd";
    let dialect = IndexDialect {
        bom_len: 3,
        ..utf8(b',')
    };
    let index = RowIndex::build(bytes, dialect).unwrap();
    // The quote straight after the BOM opens a quoted field.
    assert_eq!(index.row(0, bytes).unwrap().span, 3..10);
    assert_eq!(index.row(1, bytes).unwrap().span, 11..12);
}

#[test]
fn a_final_line_ending_ends_the_last_row() {
    assert_eq!(rows_of(b"a\nb\n"), [(0..1, Some(Lf)), (2..3, Some(Lf))]);
    assert_eq!(rows_of(b"a\nb"), [(0..1, Some(Lf)), (2..3, None)]);
    // ...and a blank line after it is a row (ADR-0003 decision 5).
    assert_eq!(
        rows_of(b"a\nb\n\n"),
        [(0..1, Some(Lf)), (2..3, Some(Lf)), (4..4, Some(Lf))]
    );
    assert_eq!(rows_of(b"\n"), [(0..0, Some(Lf))]);
}

#[test]
fn lf_crlf_and_lone_cr_each_end_a_row() {
    assert_eq!(
        rows_of(b"a,b\r\nc\rd\ne"),
        [
            (0..3, Some(Crlf)),
            (5..6, Some(Cr)),
            (7..8, Some(Lf)),
            (9..10, None)
        ]
    );
    assert_eq!(
        rows_of(b"\r\r\n\r"),
        [(0..0, Some(Cr)), (1..1, Some(Crlf)), (3..3, Some(Cr))]
    );
}

#[test]
fn newlines_inside_quotes_do_not_end_rows() {
    let bytes = b"\"x\ny\",1\n2,\"a\r\nb\rc\"\n3";
    assert_eq!(
        rows_of(bytes),
        [(0..7, Some(Lf)), (8..18, Some(Lf)), (19..20, None)]
    );
}

#[test]
fn doubled_quotes_are_escapes_inside_quotes() {
    // `"a""` then a newline: still inside the field.
    assert_eq!(
        rows_of(b"\"a\"\"\nb\",c\nd"),
        [(0..9, Some(Lf)), (10..11, None)]
    );
    // `""` as a whole field is empty, not an escape.
    assert_eq!(rows_of(b"\"\",\"\"\nx"), [(0..5, Some(Lf)), (6..7, None)]);
}

#[test]
fn a_quote_inside_an_unquoted_field_is_literal() {
    assert_eq!(
        rows_of(b"a\"b\nc\"d\n"),
        [(0..3, Some(Lf)), (4..7, Some(Lf))]
    );
}

#[test]
fn a_quote_after_a_delimiter_opens_a_field() {
    assert_eq!(rows_of(b"a,\"b\nc\"\nd"), [(0..7, Some(Lf)), (8..9, None)]);
}

/// ADR-0003 decision 3: `"a"b"c` is one field, and the quote after `b` is
/// literal, so it doesn't reopen quoting.
#[test]
fn quotes_in_text_after_a_closing_quote_are_literal() {
    assert_eq!(
        rows_of(b"\"a\"b\"c\nd,e\n"),
        [(0..6, Some(Lf)), (7..10, Some(Lf))]
    );
    assert_eq!(field_counts(b"\"a\"b\"c\",d\n", utf8(b',')), [2]);
}

#[test]
fn an_unterminated_quote_runs_to_the_end_of_the_file() {
    let bytes = b"a\n\"b\nc\n";
    let index = RowIndex::build(bytes, utf8(b',')).unwrap();
    assert_eq!(index.row_count(), 2);
    assert_eq!(index.unterminated_quote(), Some(2));
    // The final newline is inside the field, so it isn't a line ending.
    assert_eq!(
        index.row(1, bytes),
        Some(RowSpan {
            span: 2..7,
            line_ending: None
        })
    );
    // An opening quote as the very last byte.
    let index = RowIndex::build(b"a,\"", utf8(b',')).unwrap();
    assert_eq!(index.unterminated_quote(), Some(2));
    assert_eq!(index.row(0, b"a,\"").unwrap().span, 0..3);
}

#[test]
fn row_extents_tile_the_file_after_the_bom() {
    let bytes = b"\xEF\xBB\xBFa\r\n\nb";
    let index = RowIndex::build(
        bytes,
        IndexDialect {
            bom_len: 3,
            ..utf8(b',')
        },
    )
    .unwrap();
    let extents: Vec<_> = (0..index.row_count())
        .map(|r| index.row_extent(r).unwrap())
        .collect();
    assert_eq!(extents, [3..6, 6..7, 7..8]);
    assert_eq!(index.row_extent(3), None);
}

// ---------------------------------------------------------------------------
// Field counts

#[test]
fn field_counts_skip_delimiters_inside_quotes() {
    assert_eq!(
        field_counts(b"a,b,c\n\"x,y\",z\n,,\n\"\"\n", utf8(b',')),
        [3, 2, 3, 1]
    );
}

#[test]
fn the_dominant_field_count_prefers_the_first_seen_on_ties() {
    let mode = |bytes: &[u8]| {
        RowIndex::build(bytes, utf8(b','))
            .unwrap()
            .field_count_mode()
    };
    assert_eq!(mode(b"a,b\nc\nd,e\nf\n"), Some(2));
    assert_eq!(mode(b"c\na,b\nd,e\nf\n"), Some(1));
    assert_eq!(mode(b"c\na,b\nd,e\n"), Some(2));
    // Blank lines are left out (ADR-0003 decision 4)...
    assert_eq!(mode(b"\n\n\na,b\n"), Some(2));
    // ...so a file of only blank lines has none.
    assert_eq!(mode(b"\n\r\n"), None);
    // A quoted empty field is not a blank line.
    assert_eq!(mode(b"\"\"\n\"\"\na,b\n"), Some(1));
}

/// Re-indexing the same bytes with another delimiter changes both the
/// field counts and, because a quote opens a field only after a delimiter,
/// the rows (PLAN 1.3: no reopen needed).
#[test]
fn reindexing_with_another_delimiter_changes_the_rows() {
    let bytes = b"a;\"b\nc\",d\n";
    let comma = RowIndex::build(bytes, utf8(b',')).unwrap();
    let semicolon = RowIndex::build(bytes, utf8(b';')).unwrap();
    assert_eq!(comma.row_count(), 2);
    assert_eq!(semicolon.row_count(), 1);
    assert_eq!(comma.field_count_mode(), Some(1));
    assert_eq!(semicolon.field_count_mode(), Some(2));
}

// ---------------------------------------------------------------------------
// UTF-16

/// Characters whose code units contain the bytes of `"`, `,`, CR and LF in
/// the other half: none of them is structure.
const TRICKY_UTF16: &str = "\u{220A}\u{0A22}\u{0D2C}\u{2C0D}\u{2222}\u{0A0A}";

#[test]
fn utf16_rows_split_on_code_units_not_bytes() {
    for le in [true, false] {
        let text = format!("{TRICKY_UTF16},\"q\n{TRICKY_UTF16}\"\r\n{TRICKY_UTF16}\n\n😀");
        let bytes = utf16(&text, le);
        let (index, facts) = index_with(&bytes, utf16_dialect(le), CHUNK_BYTES);
        let units = TRICKY_UTF16.encode_utf16().count() * 2;
        // TRICKY `,"q LF` TRICKY `"`: 4 units between the two.
        let row0_end = 2 + units + 8 + units + 2;
        let row1 = row0_end + 4..row0_end + 4 + units;
        assert_eq!(
            spans(&index, &bytes),
            [
                RowSpan {
                    span: 2..row0_end,
                    line_ending: Some(Crlf)
                },
                RowSpan {
                    span: row1.clone(),
                    line_ending: Some(Lf)
                },
                RowSpan {
                    span: row1.end + 2..row1.end + 2,
                    line_ending: Some(Lf)
                },
                RowSpan {
                    span: row1.end + 4..bytes.len(),
                    line_ending: None
                },
            ],
            "little endian: {le}"
        );
        let counts: Vec<_> = facts.iter().map(|r| r.fields).collect();
        assert_eq!(counts, [2, 1, 1, 1], "little endian: {le}");
    }
}

/// A final odd byte is part of the last row (the testkit's convention: it
/// is one invalid character). A 0x0A there is not a line ending.
#[test]
fn a_final_odd_utf16_byte_belongs_to_the_last_row() {
    for le in [true, false] {
        let mut bytes = utf16("a\n", le);
        bytes.push(b'\n');
        let index = RowIndex::build(&bytes, utf16_dialect(le)).unwrap();
        assert_eq!(index.row_count(), 2, "little endian: {le}");
        assert_eq!(index.row(1, &bytes).unwrap().span, 6..7);
        assert_eq!(index.row(1, &bytes).unwrap().line_ending, None);
    }
}

#[test]
fn utf16_unterminated_quote() {
    for le in [true, false] {
        let bytes = utf16("a\n\"b\n", le);
        let index = RowIndex::build(&bytes, utf16_dialect(le)).unwrap();
        assert_eq!(index.unterminated_quote(), Some(6));
        assert_eq!(index.row(1, &bytes).unwrap().span, 6..bytes.len());
    }
}

// ---------------------------------------------------------------------------
// Errors

#[test]
fn the_delimiter_and_quote_must_be_usable() {
    for (delimiter, quote) in [(b',', b','), (b'\n', b'"'), (b',', b'\r'), (0xA7, b'"')] {
        let dialect = IndexDialect {
            delimiter,
            quote,
            ..utf8(b',')
        };
        assert_eq!(
            RowIndex::build(b"a", dialect).unwrap_err(),
            IndexError::InvalidDialect { delimiter, quote }
        );
        assert!(RowIndex::start(dialect).is_err());
    }
}

#[test]
fn a_bom_longer_than_the_file_is_an_error() {
    let dialect = IndexDialect {
        bom_len: 3,
        ..utf8(b',')
    };
    assert_eq!(
        RowIndex::build(b"ab", dialect).unwrap_err(),
        IndexError::BomPastEnd { bom_len: 3, len: 2 }
    );
}

/// Files of 4 GiB or more don't fit `u32` offsets. Checked on the length
/// alone: the test can't allocate 4 GiB.
#[test]
fn files_over_4_gib_are_refused() {
    assert_eq!(check_len(MAX_FILE_BYTES), Ok(()));
    assert_eq!(
        check_len(MAX_FILE_BYTES + 1),
        Err(IndexError::TooLarge {
            len: MAX_FILE_BYTES + 1
        })
    );
    assert_eq!(MAX_FILE_BYTES, 4 * 1024 * 1024 * 1024 - 1);
}

#[test]
fn errors_explain_themselves() {
    let text = IndexError::TooLarge { len: 5 }.to_string();
    assert!(text.contains("4294967295"), "{text}");
    let text = IndexError::InvalidDialect {
        delimiter: b',',
        quote: b',',
    }
    .to_string();
    assert!(text.contains("','"), "{text}");
}

// ---------------------------------------------------------------------------
// Progress, chunks and cancellation

/// A file of `rows` rows, some with quoted newlines, about 20 bytes each.
fn sample_file(rows: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..rows {
        if i % 7 == 3 {
            bytes.extend_from_slice(format!("{i},\"line\nbreak\",x\r\n").as_bytes());
        } else {
            bytes.extend_from_slice(format!("{i},plain,\"q\"\"q\"\n").as_bytes());
        }
    }
    bytes
}

#[test]
fn progress_is_reported_after_every_chunk() {
    let bytes = sample_file(1000);
    let (index, mut indexer) = RowIndex::start(utf8(b',')).unwrap();
    indexer.chunk_bytes = 1000;
    let mut seen = Vec::new();
    indexer
        .run(&bytes, &AtomicBool::new(false), |p| seen.push(p))
        .unwrap();
    assert_eq!(seen.len(), bytes.len().div_ceil(1000));
    for (i, p) in seen.iter().enumerate() {
        // Chunk i ends at (i + 1) × 1000, or one byte later if a CRLF or
        // `""` straddles the boundary.
        let boundary = ((i + 1) * 1000).min(bytes.len());
        assert!(
            (boundary..=boundary + 1).contains(&p.bytes_scanned),
            "{p:?}"
        );
    }
    for pair in seen.windows(2) {
        assert!(pair[0].rows <= pair[1].rows);
    }
    let last = seen.last().unwrap();
    assert_eq!(last.rows, 1000);
    assert_eq!(last.bytes_scanned, bytes.len());
    assert_eq!(last.bytes_total, bytes.len());
    assert_eq!(index.status(), Status::Complete);
    assert_eq!(index.row_count(), 1000);
}

/// Rows are visible as soon as their chunk is published, and only rows
/// whose end is known are visible.
#[test]
fn rows_appear_chunk_by_chunk() {
    let bytes = sample_file(500);
    let (index, mut indexer) = RowIndex::start(utf8(b',')).unwrap();
    indexer.chunk_bytes = 512;
    let reader = Arc::clone(&index);
    let mut checked = 0;
    indexer
        .run(&bytes, &AtomicBool::new(false), |p| {
            assert_eq!(reader.row_count(), p.rows);
            assert_eq!(reader.bytes_scanned(), p.bytes_scanned);
            if p.bytes_scanned < bytes.len() {
                assert_eq!(reader.status(), Status::Indexing);
                // The last visible row ends at or before the scan position.
                let last = reader.row_extent(p.rows - 1).unwrap();
                assert!(last.end <= p.bytes_scanned);
                assert_eq!(reader.row_extent(p.rows), None);
                checked += 1;
            }
        })
        .unwrap();
    assert!(checked > 10);
    // Every row seen early is the row in the complete index.
    let full = RowIndex::build(&bytes, utf8(b',')).unwrap();
    assert_eq!(spans(&index, &bytes), spans(&full, &bytes));
}

#[test]
fn a_reader_on_another_thread_sees_rows_grow() {
    let bytes: Arc<[u8]> = Arc::from(sample_file(50_000));
    let (index, mut indexer) = RowIndex::start(utf8(b',')).unwrap();
    indexer.chunk_bytes = 4096;
    let worker = std::thread::spawn({
        let bytes = Arc::clone(&bytes);
        move || indexer.run(&bytes, &AtomicBool::new(false), |_| {})
    });
    let mut counts = Vec::new();
    while index.status() == Status::Indexing {
        let rows = index.row_count();
        if let Some(r) = rows.checked_sub(1) {
            // The newest visible row is readable and complete.
            let row = index.row(r, &bytes).unwrap();
            assert!(row.line_ending.is_some());
        }
        counts.push(rows);
    }
    worker.join().unwrap().unwrap();
    assert!(counts.windows(2).all(|w| w[0] <= w[1]), "never shrinks");
    assert_eq!(index.row_count(), 50_000);
}

#[test]
fn a_cancel_before_the_first_chunk_does_no_work() {
    let bytes = sample_file(100);
    let (index, indexer) = RowIndex::start(utf8(b',')).unwrap();
    let cancel = AtomicBool::new(true);
    let mut calls = 0;
    assert_eq!(
        indexer.run(&bytes, &cancel, |_| calls += 1),
        Err(IndexError::Cancelled)
    );
    assert_eq!(calls, 0);
    assert_eq!(index.status(), Status::Stopped);
    assert_eq!(index.row_count(), 0);
}

/// Cancelling stops the run at the next chunk boundary: no chunk after the
/// one that was running is scanned (ADR-0005 decision 6).
#[test]
fn cancelling_stops_within_one_chunk() {
    let bytes = sample_file(1000);
    let (index, mut indexer) = RowIndex::start(utf8(b',')).unwrap();
    indexer.chunk_bytes = 1000;
    let cancel = AtomicBool::new(false);
    let mut seen = Vec::new();
    let result = indexer.run(&bytes, &cancel, |p| {
        seen.push(p);
        if seen.len() == 3 {
            cancel.store(true, Ordering::Relaxed);
        }
    });
    assert_eq!(result, Err(IndexError::Cancelled));
    assert_eq!(seen.len(), 3);
    assert_eq!(index.status(), Status::Stopped);
    assert!((3000..=3001).contains(&index.bytes_scanned()));
    assert_eq!(index.row_count(), seen[2].rows);
    // The rows published before the cancel are still right.
    let full = RowIndex::build(&bytes, utf8(b',')).unwrap();
    for r in 0..index.row_count() {
        assert_eq!(index.row(r, &bytes), full.row(r, &bytes));
    }
}

#[test]
fn an_indexer_dropped_without_running_stops_the_index() {
    let (index, indexer) = RowIndex::start(utf8(b',')).unwrap();
    assert_eq!(index.status(), Status::Indexing);
    drop(indexer);
    assert_eq!(index.status(), Status::Stopped);
}

#[test]
fn a_failed_run_stops_the_index() {
    let (index, indexer) = RowIndex::start(IndexDialect {
        bom_len: 9,
        ..utf8(b',')
    })
    .unwrap();
    assert!(indexer.run(b"a", &AtomicBool::new(false), |_| {}).is_err());
    assert_eq!(index.status(), Status::Stopped);
}

#[test]
fn the_row_count_estimate_comes_from_the_average_row_so_far() {
    let bytes = b"aaaa\nbbbb\ncccc\ndddd\n";
    let (index, mut indexer) = RowIndex::start(utf8(b',')).unwrap();
    assert_eq!(index.estimated_row_count(), None);
    indexer.chunk_bytes = 6;
    let reader = Arc::clone(&index);
    let mut estimates = Vec::new();
    indexer
        .run(bytes, &AtomicBool::new(false), |_| {
            estimates.push(reader.estimated_row_count());
        })
        .unwrap();
    // After 6 bytes, one 5-byte row: 20 bytes ÷ 5 = 4 rows.
    assert_eq!(estimates[0], Some(4));
    assert_eq!(estimates.last(), Some(&Some(4)));
    assert_eq!(
        RowIndex::build(b"", utf8(b','))
            .unwrap()
            .estimated_row_count(),
        None
    );
}

#[test]
fn the_index_can_be_shared_between_threads() {
    fn shareable<T: Send + Sync>() {}
    shareable::<RowIndex>();
    fn sendable<T: Send>() {}
    sendable::<Indexer>();
}

/// Chunk boundaries can fall anywhere: inside CRLF, inside `""`, between a
/// delimiter and a quote. Every chunk size gives the same index.
#[test]
fn every_chunk_size_gives_the_same_index() {
    let bytes = b"\"a\"\"b\",\"\r\n\"\r\n,\"x\"y\"\r\r\n\n\"\"\"";
    let (whole, whole_facts) = index_with(bytes, utf8(b','), CHUNK_BYTES);
    for chunk in 1..=bytes.len() {
        let (index, facts) = index_with(bytes, utf8(b','), chunk);
        assert_eq!(spans(&index, bytes), spans(&whole, bytes), "chunk {chunk}");
        assert_eq!(facts, whole_facts, "chunk {chunk}");
        assert_eq!(index.unterminated_quote(), whole.unterminated_quote());
    }
    for le in [true, false] {
        let bytes = utf16(&format!("\"{TRICKY_UTF16}\"\"\",\"\r\n\"\r\nx\r"), le);
        let (whole, _) = index_with(&bytes, utf16_dialect(le), CHUNK_BYTES);
        for chunk in 1..=bytes.len() {
            let (index, _) = index_with(&bytes, utf16_dialect(le), chunk);
            assert_eq!(
                spans(&index, &bytes),
                spans(&whole, &bytes),
                "chunk {chunk}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Against the testkit's generated files

proptest! {
    #[test]
    fn clean_files_match_the_model(file in csv_file(CsvConfig::clean())) {
        check_generated(&file, CHUNK_BYTES)?;
    }

    #[test]
    fn messy_files_match_the_model(file in csv_file(CsvConfig::messy())) {
        check_generated(&file, CHUNK_BYTES)?;
    }

    #[test]
    fn clean_utf16_files_match_the_model(file in csv_file_utf16(CsvConfig::clean())) {
        check_generated(&file, CHUNK_BYTES)?;
    }

    #[test]
    fn messy_utf16_files_match_the_model(file in csv_file_utf16(CsvConfig::messy())) {
        check_generated(&file, CHUNK_BYTES)?;
    }

    /// Small chunks put boundaries everywhere in the file.
    #[test]
    fn messy_files_match_the_model_in_small_chunks(
        file in csv_file(CsvConfig::messy()),
        chunk in 1usize..16,
    ) {
        check_generated(&file, chunk)?;
    }

    #[test]
    fn messy_utf16_files_match_the_model_in_small_chunks(
        file in csv_file_utf16(CsvConfig::messy()),
        chunk in 1usize..16,
    ) {
        check_generated(&file, chunk)?;
    }

    /// Any bytes at all: the extents tile the file, every row but the last
    /// ends in a line ending, and the chunk size makes no difference.
    #[test]
    fn any_bytes_tile_and_ignore_chunk_size(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&b",;\t|"[..]),
        chunk in 1usize..32,
    ) {
        let dialect = utf8(delimiter);
        let (whole, whole_facts) = index_with(&bytes, dialect, CHUNK_BYTES);
        let (index, facts) = index_with(&bytes, dialect, chunk);
        prop_assert_eq!(spans(&index, &bytes), spans(&whole, &bytes));
        prop_assert_eq!(facts, whole_facts);
        prop_assert_eq!(index.field_count_mode(), whole.field_count_mode());
        let mut pos = 0;
        for r in 0..index.row_count() {
            let extent = index.row_extent(r).unwrap();
            prop_assert_eq!(extent.start, pos);
            prop_assert!(extent.end > extent.start);
            pos = extent.end;
            let row = index.row(r, &bytes).unwrap();
            if r + 1 < index.row_count() {
                prop_assert!(row.line_ending.is_some());
            }
        }
        prop_assert_eq!(pos, bytes.len());
    }
}

// ---------------------------------------------------------------------------
// Against the corpus sidecars

fn corpus_dialect(case: &CorpusCase) -> IndexDialect {
    let d = &case.sidecar.dialect;
    dialect_for(d.delimiter.byte(), d.encoding, d.bom.bytes().len())
}

/// The locations a sidecar lists for `kind`, as `(row, offset)`.
fn locations(case: &CorpusCase, kind: DiagnosticKind) -> Vec<(usize, usize)> {
    case.sidecar
        .diagnostic(kind)
        .map(|d| d.first.iter().map(|l| (l.row, l.offset)).collect())
        .unwrap_or_default()
}

/// Every sidecar's row count, field counts, dominant field count, trailing
/// newline and line endings, and the offsets of its row-level diagnostics:
/// blank lines and ragged rows (row starts), mixed line endings (where the
/// line ending starts) and the unterminated quote.
#[test]
fn every_corpus_sidecar_matches() {
    let cases = leal_testkit::corpus::load().unwrap();
    assert!(cases.len() >= 40);
    for case in &cases {
        let name = &case.name;
        let bytes = &case.bytes;
        let expected = &case.sidecar;
        let (index, facts) = index_with(bytes, corpus_dialect(case), CHUNK_BYTES);
        let rows = spans(&index, bytes);

        assert_eq!(index.row_count(), expected.rows.count, "{name}: rows");
        let counts: Vec<usize> = facts.iter().map(|r| r.fields).collect();
        assert_eq!(counts, expected.rows.field_counts(), "{name}: field counts");
        let trailing = rows.last().is_some_and(|r| r.line_ending.is_some());
        assert_eq!(
            trailing, expected.dialect.trailing_newline,
            "{name}: trailing newline"
        );

        let blank: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.span.is_empty())
            .map(|(i, r)| (i, r.span.start))
            .collect();
        assert_eq!(
            blank,
            locations(case, DiagnosticKind::BlankLines),
            "{name}: blank lines"
        );

        let mode = index.field_count_mode();
        if let Some(m) = expected.rows.fields_mode {
            assert_eq!(mode, Some(m), "{name}: field-count mode");
        }
        let ragged: Vec<_> = rows
            .iter()
            .zip(&counts)
            .enumerate()
            .filter(|(_, (r, c))| !r.span.is_empty() && Some(**c) != mode)
            .map(|(i, (r, _))| (i, r.span.start))
            .collect();
        assert_eq!(
            ragged,
            locations(case, DiagnosticKind::RaggedRows),
            "{name}: ragged rows"
        );

        // The most common line ending, ties to the first seen.
        let endings: Vec<LineEnding> = rows.iter().filter_map(|r| r.line_ending).collect();
        let mut tally: Vec<(LineEnding, usize)> = Vec::new();
        for e in &endings {
            match tally.iter_mut().find(|(t, _)| t == e) {
                Some((_, n)) => *n += 1,
                None => tally.push((*e, 1)),
            }
        }
        let dominant = tally
            .iter()
            .fold(
                None,
                |best: Option<(LineEnding, usize)>, &(e, n)| match best {
                    Some((_, b)) if b >= n => best,
                    _ => Some((e, n)),
                },
            )
            .map(|(e, _)| e);
        assert_eq!(
            dominant,
            expected.dialect.line_ending.map(tk),
            "{name}: line ending"
        );
        let mixed: Vec<_> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.line_ending.is_some() && r.line_ending != dominant)
            .map(|(i, r)| (i, r.span.end))
            .collect();
        assert_eq!(
            !mixed.is_empty(),
            expected.dialect.mixed_line_endings,
            "{name}: mixed line endings"
        );
        assert_eq!(
            mixed,
            locations(case, DiagnosticKind::MixedLineEndings),
            "{name}: mixed line ending offsets"
        );

        let unterminated = locations(case, DiagnosticKind::UnterminatedQuote);
        assert_eq!(
            index.unterminated_quote(),
            unterminated.first().map(|&(_, offset)| offset),
            "{name}: unterminated quote"
        );
        if let Some(&(row, _)) = unterminated.first() {
            assert_eq!(
                row + 1,
                index.row_count(),
                "{name}: unterminated quote's row"
            );
        }
    }
}
