//! The row index on arbitrary bytes, in every delimiter and in each
//! encoding the bytes can be read in (`readings`): it never panics, and it
//! agrees with the testkit's reference parser on every row's span and line
//! ending, the field-count mode, the unterminated quote and the row at
//! every offset. Built in one go, chunk by chunk (chunk sizes from 1 byte),
//! and with diagnostics, which must be the reference parser's too.

#![no_main]

use std::sync::atomic::AtomicBool;

use leal_core::dialect::Delimiter;
use leal_core::index::{RowIndex, Status};
use leal_fuzz::{index_dialect, oracle, readings, tk_delimiter, tk_diagnostics, tk_encoding};
use leal_testkit::layout::Layout;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    for (encoding, bom_len) in readings(bytes) {
        for delimiter in Delimiter::ALL {
            let dialect = index_dialect(delimiter, encoding, bom_len);
            let analysis = oracle::analyze(bytes, tk_delimiter(delimiter), tk_encoding(encoding));
            let what = format!("{delimiter:?} {encoding:?}");

            let whole = RowIndex::build(bytes, dialect).expect("any bytes index");
            check(&whole, bytes, &analysis.layout, &what);

            // Chunk by chunk (as a removable drive's file is read), with a
            // chunk size the bytes pick, collecting diagnostics.
            let chunk = 1 + bytes.len() % 61;
            let (chunked, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(dialect, encoding).expect("a usable dialect");
            let mut indexer = indexer.chunked(bytes.len()).expect("the BOM fits");
            for part in bytes.chunks(chunk) {
                indexer.push(part).expect("no more than the length");
            }
            indexer.finish().expect("all the bytes were pushed");
            let what_chunked = format!("{what}, chunks of {chunk}");
            check(&chunked, bytes, &analysis.layout, &what_chunked);
            let expected = &analysis.diagnostics;
            let report = diagnostics.report();
            assert!(report.is_complete(), "{what_chunked}: diagnostics complete");
            assert_eq!(
                &tk_diagnostics(&report),
                expected,
                "{what_chunked}: diagnostics"
            );

            // The background indexer's path, with diagnostics.
            let (background, diagnostics, indexer) =
                RowIndex::start_with_diagnostics(dialect, encoding).expect("a usable dialect");
            indexer
                .run(bytes, &AtomicBool::new(false), |_| {})
                .expect("never cancelled");
            check(
                &background,
                bytes,
                &analysis.layout,
                &format!("{what}, run"),
            );
            assert_eq!(
                &tk_diagnostics(&diagnostics.report()),
                expected,
                "{what}: run diagnostics"
            );
        }
    }
});

/// The index against the reference parser's layout.
fn check(index: &RowIndex, bytes: &[u8], layout: &Layout, what: &str) {
    assert_eq!(index.status(), Status::Complete, "{what}");
    assert_eq!(index.row_count(), layout.rows.len(), "{what}: row count");
    let mut end = layout.bom_len;
    for (r, expected) in layout.rows.iter().enumerate() {
        let row = index.row(r, bytes).expect("an indexed row");
        assert_eq!(row.span, expected.span, "{what}: row {r} span");
        assert_eq!(
            row.line_ending.map(leal_fuzz::tk_line_ending),
            expected.line_ending,
            "{what}: row {r} line ending"
        );
        // Row extents tile the file after the BOM.
        let extent = index.row_extent(r).expect("an indexed row");
        assert_eq!(
            extent.start, end,
            "{what}: row {r} starts where the last ended"
        );
        assert!(extent.end > extent.start, "{what}: row {r} is not empty");
        end = extent.end;
    }
    if !layout.rows.is_empty() {
        assert_eq!(
            end,
            bytes.len(),
            "{what}: the rows end at the end of the file"
        );
    }
    assert_eq!(
        index.field_count_mode(),
        layout.field_count_mode(),
        "{what}: field-count mode"
    );
    let unterminated = layout
        .rows
        .last()
        .and_then(|r| r.fields.last())
        .filter(|f| f.unterminated)
        .map(|f| f.span.start);
    assert_eq!(
        index.unterminated_quote(),
        unterminated,
        "{what}: unterminated quote"
    );
    // The row at each offset, its line ending included. Not the testkit's
    // `Layout::row_of_offset`, which counts a line ending's characters, not
    // its UTF-16 bytes.
    let width = index.dialect().code_unit.width();
    for offset in 0..=bytes.len() + 1 {
        let after = layout.rows.partition_point(|r| r.span.start <= offset);
        let expected = after.checked_sub(1).filter(|&r| {
            let row = &layout.rows[r];
            offset < row.span.end + row.line_ending.map_or(0, |l| l.byte_len() * width)
        });
        assert_eq!(
            index.row_at_offset(offset),
            expected,
            "{what}: row at offset {offset}"
        );
    }
}
