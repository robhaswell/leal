//! Rows and fields (PLAN 1.4): reading one screenful of cells from the
//! reference file, which DESIGN §3.9 says must take under 1 ms, since the
//! grid does it on the main thread.
//!
//! A screenful is 60 rows of the reference file's 12 columns: 720 cells,
//! from the middle of the file. The index is built once, outside the
//! measurement, as it is by the time the user scrolls there.
//!
//! - `rows/screen`: parse the 60 rows and derive every display value, with
//!   no cache, as when the user jumps to a new place in the file.
//! - `rows/screen_cached`: the same through a [`RowCache`] that already
//!   holds the rows, as when the grid redraws or scrolls a little.
//! - `rows/parse`: parsing alone, without display values.
//! - `rows/screen_long_field_{utf8,escaped,utf16}`: 59 rows of the
//!   reference file and one whose notes field is 1 MB, parsed uncached,
//!   with every cell read through `display_prefix` (the first 256
//!   characters), as the grid reads them. The field is plain mixed-script
//!   UTF-8, quoted with a `""` every 15 bytes or so, or the plain screen
//!   in UTF-16 LE.
//!
//! Display values are counted, not copied: the grid's FFI call (1.6) copies
//! each one into a Swift string, which isn't this crate's cost.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use leal_bench::report::Side;
use leal_core::index::{CodeUnit, IndexDialect, RowIndex};
use leal_core::rows::{DEFAULT_CACHE_ROWS, Encoding, ParsedRow, RowCache, RowParser};

// Only `reference_file` and `canary` are used here: these benchmarks take
// microseconds, so they keep criterion's default sampling rather than
// `whole_file`'s.
#[allow(dead_code)]
mod common;

/// The reference file's dialect: comma, `"`, UTF-8, no BOM.
const DIALECT: IndexDialect = IndexDialect {
    delimiter: b',',
    quote: b'"',
    code_unit: CodeUnit::Byte,
    bom_len: 0,
};

/// Rows on one screen.
const SCREEN_ROWS: usize = 60;

/// The first row on screen: the middle of the file.
const FIRST_ROW: usize = 500_000;

/// The total length of every display value on screen, so the work can't
/// be optimised away.
fn display_len(parser: &RowParser, bytes: &[u8], row: &ParsedRow) -> usize {
    row.fields()
        .iter()
        .map(|field| parser.display_value(bytes, field).len())
        .sum()
}

fn rows(c: &mut Criterion) {
    let path = common::reference_file();
    let bytes = std::fs::read(&path).expect("reading the reference file");
    let index = RowIndex::build(&bytes, DIALECT).expect("indexing");
    let parser = RowParser::new(DIALECT, Encoding::Utf8).expect("a valid dialect");
    let screen = FIRST_ROW..FIRST_ROW + SCREEN_ROWS;
    let cells = (SCREEN_ROWS * leal_bench::reference::COLUMNS) as u64;

    common::canary(c, "rows", Side::Before);
    let mut group = c.benchmark_group("rows");
    group.throughput(Throughput::Elements(cells));

    group.bench_function("screen", |b| {
        b.iter(|| {
            let mut total = 0;
            for r in screen.clone() {
                let row = parser
                    .parse_row(&index, black_box(r), &bytes)
                    .expect("an indexed row");
                assert_eq!(row.fields().len(), leal_bench::reference::COLUMNS);
                total += display_len(&parser, &bytes, &row);
            }
            total
        });
    });

    let mut cache = RowCache::new(parser, DEFAULT_CACHE_ROWS);
    for r in screen.clone() {
        cache.row(&index, r, &bytes).expect("an indexed row");
    }
    group.bench_function("screen_cached", |b| {
        b.iter(|| {
            let mut total = 0;
            for r in screen.clone() {
                let row = cache
                    .row(&index, black_box(r), &bytes)
                    .expect("an indexed row");
                total += display_len(&parser, &bytes, &row);
            }
            total
        });
    });

    group.bench_function("parse", |b| {
        b.iter(|| {
            let mut fields = 0;
            for r in screen.clone() {
                let row = parser
                    .parse_row(&index, black_box(r), &bytes)
                    .expect("an indexed row");
                fields += row.fields().len();
            }
            fields
        });
    });

    // A screen whose last row has a 1 MB notes field. The grid reads cells
    // with `display_prefix`, so only the start of that field is decoded.
    let screen_bytes = &bytes[index.row_extent(FIRST_ROW).expect("indexed").start
        ..index
            .row_extent(FIRST_ROW + SCREEN_ROWS - 2)
            .expect("indexed")
            .end];
    for (name, encoding, notes) in [
        ("screen_long_field_utf8", Encoding::Utf8, long_notes(false)),
        (
            "screen_long_field_escaped",
            Encoding::Utf8,
            long_notes(true),
        ),
        (
            "screen_long_field_utf16",
            Encoding::Utf16Le,
            long_notes(false),
        ),
    ] {
        let mut file = screen_bytes.to_vec();
        file.extend_from_slice(LONG_ROW_START.as_bytes());
        file.extend_from_slice(notes.as_bytes());
        file.push(b'\n');
        let (file, dialect) = match encoding {
            Encoding::Utf16Le => (
                utf16le(&file),
                IndexDialect {
                    code_unit: CodeUnit::Utf16Le,
                    bom_len: 2,
                    ..DIALECT
                },
            ),
            _ => (file, DIALECT),
        };
        let index = RowIndex::build(&file, dialect).expect("indexing");
        assert_eq!(index.row_count(), SCREEN_ROWS);
        let parser = RowParser::new(dialect, encoding).expect("a valid dialect");
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut total = 0;
                for r in 0..SCREEN_ROWS {
                    let row = parser
                        .parse_row(&index, black_box(r), &file)
                        .expect("an indexed row");
                    assert_eq!(row.fields().len(), leal_bench::reference::COLUMNS);
                    for field in row.fields() {
                        total += parser.display_prefix(&file, field, GRID_CHARS).0.len();
                    }
                }
                total
            });
        });
    }

    group.finish();
    common::canary(c, "rows", Side::After);
}

/// How much of a cell the grid decodes: more than a 260 px column can show
/// (ADR-0002) at any font size.
const GRID_CHARS: usize = 256;

/// The long row's first 11 fields, in the reference file's columns.
const LONG_ROW_START: &str =
    "1000001,2020-01-01,\"Silva, Ana\",São Paulo,BR,SKU-00001,1,9.99,BRL,,shipped,";

/// A notes field of about 1 MB: unquoted mixed-script text, or quoted with
/// an escaped quote (`""`) every 15 bytes or so.
fn long_notes(escaped: bool) -> String {
    let piece = if escaped {
        "said \"\"hi\"\", ok; "
    } else {
        "São Paulo 東京 Αθήνα ok. "
    };
    let mut notes = String::new();
    if escaped {
        notes.push('"');
    }
    while notes.len() < 1 << 20 {
        notes.push_str(piece);
    }
    if escaped {
        notes.push('"');
    }
    notes
}

/// UTF-8 text as UTF-16 LE with a BOM.
fn utf16le(utf8: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(utf8).expect("UTF-8");
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

criterion_group!(benches, rows);
criterion_main!(benches);
