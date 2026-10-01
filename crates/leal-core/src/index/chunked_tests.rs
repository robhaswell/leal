//! Tests for the index of a file read in chunks ([`Indexer::chunked`]) and
//! for rows read from a window of the file ([`RowIndex::row_in`]), task
//! 1.3a.
//!
//! The chunked index must be exactly the index [`RowIndex::build`] gives
//! over the whole file, wherever the chunks are cut: the same row extents,
//! field-count mode, unterminated quote, and the same facts for every row
//! (so the diagnostics hook, 1.5, sees the same rows). `build` itself is
//! checked against the testkit's layouts in `tests.rs`.

use super::scan::{FieldCounts, RowFacts, RowObserver};
use super::*;

use leal_testkit::dialect::Encoding as TkEncoding;
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, csv_file, csv_file_utf16};
use proptest::prelude::*;

/// Collects what the scanner reports for each row.
#[derive(Default)]
struct Rows(Vec<RowFacts>);

impl RowObserver for Rows {
    fn row(&mut self, facts: &RowFacts, _counts: &FieldCounts) {
        self.0.push(facts.clone());
    }
}

fn dialect(delimiter: u8, code_unit: CodeUnit, bom_len: usize) -> IndexDialect {
    IndexDialect {
        delimiter,
        quote: b'"',
        code_unit,
        bom_len,
    }
}

/// Everything readers can see of a complete index, and every row's facts.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    extents: Vec<Range<usize>>,
    spans: Vec<RowSpan>,
    field_count_mode: Option<usize>,
    unterminated_quote: Option<usize>,
    status: Status,
    facts: Vec<RowFacts>,
}

fn seen(index: &RowIndex, bytes: &[u8], facts: Vec<RowFacts>) -> Seen {
    let rows = index.row_count();
    Seen {
        extents: (0..rows).map(|r| index.row_extent(r).unwrap()).collect(),
        spans: (0..rows).map(|r| index.row(r, bytes).unwrap()).collect(),
        field_count_mode: index.field_count_mode(),
        unterminated_quote: index.unterminated_quote(),
        status: index.status(),
        facts,
    }
}

/// The index over the whole file, on this thread.
fn whole(bytes: &[u8], dialect: IndexDialect) -> Seen {
    let index = RowIndex::new(dialect).unwrap();
    let mut rows = Rows::default();
    fill(
        &index,
        bytes,
        &AtomicBool::new(false),
        CHUNK_BYTES,
        |_| {},
        &mut rows,
    )
    .unwrap();
    seen(&index, bytes, rows.0)
}

/// The index from chunks of the given sizes (repeated until the file runs
/// out), checking the progress after each one.
fn chunked(bytes: &[u8], dialect: IndexDialect, sizes: &[usize]) -> Seen {
    let (index, indexer) = RowIndex::start(dialect).unwrap();
    let mut chunked = indexer.chunked(bytes.len()).unwrap();
    let mut rows = Rows::default();
    let mut offset = 0;
    let mut published = 0;
    for &size in sizes.iter().cycle() {
        if offset >= bytes.len() {
            break;
        }
        let end = (offset + size.max(1)).min(bytes.len());
        let progress = chunked
            .push_observed(&bytes[offset..end], &mut rows)
            .unwrap();
        offset = end;
        assert_eq!(chunked.received(), offset);
        assert_eq!(progress.bytes_total, bytes.len());
        assert!(progress.rows >= published, "the row count only grows");
        assert_eq!(progress.rows, index.row_count());
        // Only finished rows are published: the last one has a line ending.
        if let Some(last) = progress.rows.checked_sub(1) {
            let extent = index.row_extent(last).unwrap();
            assert!(
                extent.end <= offset,
                "a published row ends in what was given"
            );
        }
        published = progress.rows;
        assert_eq!(index.status(), Status::Indexing);
    }
    let progress = chunked.finish_observed(&mut rows).unwrap();
    assert_eq!(progress.rows, index.row_count());
    seen(&index, bytes, rows.0)
}

fn utf16_file(text: &str, little_endian: bool) -> Vec<u8> {
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

// ---------------------------------------------------------------------------
// Hand-written cases

#[test]
fn every_cut_of_a_tricky_file_gives_the_same_index() {
    // CRLF, a lone CR, a quoted CRLF, `""`, a quote after a delimiter, a
    // literal quote mid-field, text after a closing quote, a blank line and
    // an unterminated quote at the end.
    let bytes = b"a,\"x\r\ny\"\r\nb\rc,\"\"\"q\"\"\"\n\nd\"e,\"f\"g,h\r\n,\"open\r\n";
    let d = dialect(b',', CodeUnit::Byte, 0);
    let expected = whole(bytes, d);
    assert_eq!(expected.status, Status::Complete);
    for size in 1..=bytes.len() {
        assert_eq!(chunked(bytes, d, &[size]), expected, "chunks of {size}");
    }
    // Two cuts anywhere.
    for a in 0..bytes.len() {
        for b in a..bytes.len() {
            let sizes = [a, b - a, bytes.len() - b];
            let got = chunked_at(bytes, d, &sizes);
            assert_eq!(got, expected, "cut at {a} and {b}");
        }
    }
}

/// The index from exactly these chunks (empty ones included).
fn chunked_at(bytes: &[u8], dialect: IndexDialect, sizes: &[usize]) -> Seen {
    let (index, indexer) = RowIndex::start(dialect).unwrap();
    let mut chunked = indexer.chunked(bytes.len()).unwrap();
    let mut rows = Rows::default();
    let mut offset = 0;
    for &size in sizes {
        chunked
            .push_observed(&bytes[offset..offset + size], &mut rows)
            .unwrap();
        offset += size;
    }
    chunked.finish_observed(&mut rows).unwrap();
    seen(&index, bytes, rows.0)
}

#[test]
fn a_utf8_bom_and_utf16_files_cut_anywhere_give_the_same_index() {
    let utf8 = b"\xEF\xBB\xBF\"a\",b\r\n\"c\r\nd\",e\n".to_vec();
    let cases = [
        (utf8, dialect(b',', CodeUnit::Byte, 3)),
        (
            utf16_file("\"a\",\u{0A22}\r\n\"c\r\n\u{220A}\",e\n\"", true),
            dialect(b',', CodeUnit::Utf16Le, 2),
        ),
        (
            utf16_file("\"a\",\u{0A22}\r\n\"c\r\n\u{220A}\",e\n\"", false),
            dialect(b',', CodeUnit::Utf16Be, 2),
        ),
    ];
    for (bytes, d) in cases {
        let expected = whole(&bytes, d);
        for size in 1..=bytes.len() {
            assert_eq!(
                chunked(&bytes, d, &[size]),
                expected,
                "{d:?}, chunks of {size}"
            );
        }
        // And with a final odd byte.
        if d.code_unit != CodeUnit::Byte {
            let mut odd = bytes.clone();
            odd.push(b'x');
            let expected = whole(&odd, d);
            for size in 1..=odd.len() {
                assert_eq!(chunked(&odd, d, &[size]), expected, "odd, chunks of {size}");
            }
        }
    }
}

#[test]
fn empty_and_bom_only_files() {
    for (bytes, bom_len) in [(&b""[..], 0), (&b"\xEF\xBB\xBF"[..], 3)] {
        let d = dialect(b',', CodeUnit::Byte, bom_len);
        let (index, indexer) = RowIndex::start(d).unwrap();
        let mut chunked = indexer.chunked(bytes.len()).unwrap();
        if !bytes.is_empty() {
            chunked.push(bytes).unwrap();
        }
        let progress = chunked.finish().unwrap();
        assert_eq!(progress.rows, 0);
        assert_eq!(index.status(), Status::Complete);
        assert_eq!(index.row_count(), 0);
    }
}

#[test]
fn the_wrong_number_of_bytes_is_an_error() {
    let d = dialect(b',', CodeUnit::Byte, 0);
    let (index, indexer) = RowIndex::start(d).unwrap();
    let mut chunked = indexer.chunked(4).unwrap();
    chunked.push(b"a\nb").unwrap();
    assert_eq!(
        chunked.push(b"cd"),
        Err(IndexError::WrongLength {
            expected: 4,
            received: 5
        })
    );
    // The refused chunk wasn't scanned.
    assert_eq!(chunked.received(), 3);
    assert_eq!(
        chunked.finish(),
        Err(IndexError::WrongLength {
            expected: 4,
            received: 3
        })
    );
    assert_eq!(index.status(), Status::Stopped);
    assert_eq!(index.row_count(), 1, "rows found before the error stay");

    let (_, indexer) = RowIndex::start(dialect(b',', CodeUnit::Byte, 3)).unwrap();
    assert_eq!(
        indexer.chunked(2).map(|_| ()),
        Err(IndexError::BomPastEnd { bom_len: 3, len: 2 })
    );
    let (_, indexer) = RowIndex::start(d).unwrap();
    assert!(matches!(
        indexer.chunked(MAX_FILE_BYTES + 1),
        Err(IndexError::TooLarge { .. })
    ));
}

#[test]
fn a_chunked_indexer_dropped_part_way_stops_the_index() {
    let d = dialect(b',', CodeUnit::Byte, 0);
    let (index, indexer) = RowIndex::start(d).unwrap();
    let mut chunked = indexer.chunked(8).unwrap();
    chunked.push(b"a,b\nc,").unwrap();
    assert_eq!(index.status(), Status::Indexing);
    assert_eq!(index.estimated_row_count(), Some(2));
    drop(chunked);
    assert_eq!(index.status(), Status::Stopped);
    assert_eq!(index.row_count(), 1);
    assert_eq!(index.row(0, b"a,b\nc,d\n").map(|r| r.span), Some(0..3));
}

// ---------------------------------------------------------------------------
// Windows

/// Every row read from a window of exactly its extent, from a window of
/// that plus the rows around it, and from the whole file must be the same.
fn check_windows(bytes: &[u8], index: &RowIndex) -> Result<(), TestCaseError> {
    let rows = index.row_count();
    for r in 0..rows {
        let expected = index.row(r, bytes);
        prop_assert!(expected.is_some());
        let extent = index.row_extent(r).unwrap();
        let window = &bytes[extent.clone()];
        prop_assert_eq!(
            &index.row_in(r, window, extent.start),
            &expected,
            "row {}",
            r
        );
        // A screenful: from the row before to the row after.
        let screen = r.saturating_sub(1)..(r + 2).min(rows);
        let around = index.rows_extent(screen).unwrap();
        prop_assert_eq!(
            &index.row_in(r, &bytes[around.clone()], around.start),
            &expected
        );
        // A window one byte short at either end doesn't hold the row.
        if !extent.is_empty() {
            let short = &bytes[extent.start..extent.end - 1];
            prop_assert_eq!(index.row_in(r, short, extent.start), None);
            prop_assert_eq!(
                index.row_in(r, &bytes[extent.start + 1..extent.end], extent.start + 1),
                None
            );
        }
    }
    Ok(())
}

#[test]
fn rows_extent_covers_a_screenful() {
    let bytes = b"a\nbb\r\nccc\n";
    let index = RowIndex::build(bytes, dialect(b',', CodeUnit::Byte, 0)).unwrap();
    assert_eq!(index.rows_extent(0..3), Some(0..10));
    assert_eq!(index.rows_extent(1..2), Some(2..6));
    assert_eq!(index.rows_extent(1..1), None);
    assert_eq!(index.rows_extent(2..4), None);
    check_windows(bytes, &index).unwrap();
}

#[test]
fn an_unterminated_last_row_reads_the_same_from_a_window() {
    let bytes = b"a\n\"b\r\n";
    let index = RowIndex::build(bytes, dialect(b',', CodeUnit::Byte, 0)).unwrap();
    let extent = index.row_extent(1).unwrap();
    let row = index
        .row_in(1, &bytes[extent.clone()], extent.start)
        .unwrap();
    assert_eq!((row.span, row.line_ending), (2..6, None));
}

// ---------------------------------------------------------------------------
// Properties

/// Chunk sizes, mostly small so cuts fall everywhere, sometimes large.
fn chunk_sizes() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(prop_oneof![4 => 1..8usize, 1 => 8..200usize], 1..6)
}

proptest! {
    /// Any bytes in any of the four delimiters, cut anywhere.
    #[test]
    fn chunks_of_any_bytes_give_the_whole_file_index(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&b",;\t|"[..]),
        sizes in chunk_sizes(),
    ) {
        let d = dialect(delimiter, CodeUnit::Byte, 0);
        prop_assert_eq!(chunked(&bytes, d, &sizes), whole(&bytes, d));
        check_windows(&bytes, &RowIndex::build(&bytes, d).unwrap())?;
    }

    /// Generated clean and messy files (UTF-8 and Windows-1252, with and
    /// without a BOM), cut anywhere.
    #[test]
    fn chunks_of_generated_files_give_the_whole_file_index(
        file in csv_file(CsvConfig::messy()),
        sizes in chunk_sizes(),
    ) {
        let bom_len = file.layout.bom_len;
        let d = dialect(file.delimiter().byte(), CodeUnit::Byte, bom_len);
        prop_assert_eq!(chunked(&file.bytes, d, &sizes), whole(&file.bytes, d));
        check_windows(&file.bytes, &RowIndex::build(&file.bytes, d).unwrap())?;
    }

    /// Generated UTF-16 files, cut anywhere, including inside a code unit.
    #[test]
    fn chunks_of_utf16_files_give_the_whole_file_index(
        file in csv_file_utf16(CsvConfig::messy()),
        sizes in chunk_sizes(),
    ) {
        let code_unit = match file.encoding {
            TkEncoding::Utf16Be => CodeUnit::Utf16Be,
            _ => CodeUnit::Utf16Le,
        };
        let d = dialect(file.delimiter().byte(), code_unit, file.layout.bom_len);
        prop_assert_eq!(chunked(&file.bytes, d, &sizes), whole(&file.bytes, d));
        check_windows(&file.bytes, &RowIndex::build(&file.bytes, d).unwrap())?;
    }
}
