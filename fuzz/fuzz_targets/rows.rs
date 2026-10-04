//! The row parser and display values on arbitrary bytes, in every
//! delimiter and each encoding the bytes can be read in (`readings`): no
//! panic, and every field's span, quoting, text after its closing quote,
//! unterminated quote, value bytes and display value are the testkit's
//! reference parser's. Display prefixes are the full value's first
//! characters, and parsing from a window of the file gives the same row.

#![no_main]

use leal_core::dialect::Delimiter;
use leal_core::index::RowIndex;
use leal_core::rows::{FieldSpan, RowParser};
use leal_fuzz::{index_dialect, oracle, readings, tk_delimiter, tk_encoding};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    for (encoding, bom_len) in readings(bytes) {
        for delimiter in Delimiter::ALL {
            let dialect = index_dialect(delimiter, encoding, bom_len);
            let analysis = oracle::analyze(bytes, tk_delimiter(delimiter), tk_encoding(encoding));
            let layout = &analysis.layout;
            let parser = RowParser::new(dialect, encoding).expect("a usable dialect");
            let index = RowIndex::build(bytes, dialect).expect("any bytes index");
            assert_eq!(index.row_count(), layout.rows.len());
            for (r, expected_row) in layout.rows.iter().enumerate() {
                let what = format!("{delimiter:?} {encoding:?} row {r}");
                let row = parser.parse_row(&index, r, bytes).expect("an indexed row");
                assert_eq!(row.span(), expected_row.span, "{what}: span");
                assert_eq!(
                    row.fields().len(),
                    expected_row.fields.len(),
                    "{what}: field count"
                );
                // From a window that starts at the row, as the grid reads it.
                let base = row.span().start;
                let windowed = parser
                    .parse_row_in(&index, r, &bytes[base..], base)
                    .expect("the window holds the row");
                assert_eq!(windowed.fields(), row.fields(), "{what}: in a window");
                for (f, (field, expected)) in
                    row.fields().iter().zip(&expected_row.fields).enumerate()
                {
                    let what = format!("{what} field {f}");
                    assert_eq!(field.span(), expected.span, "{what}: span");
                    assert_eq!(field.quoted(), expected.quoted, "{what}: quoted");
                    assert_eq!(
                        field.text_after_quote(),
                        expected.text_after_quote,
                        "{what}: text after quote"
                    );
                    assert_eq!(
                        field.unterminated(),
                        expected.unterminated,
                        "{what}: unterminated"
                    );
                    if encoding.is_ascii_compatible() {
                        // UTF-16 layouts hold UTF-8 values, so only the
                        // display value is compared there.
                        assert_eq!(
                            &*parser.value_bytes(bytes, field),
                            &expected.value[..],
                            "{what}: value bytes"
                        );
                    }
                    let display = parser.display_value(bytes, field);
                    assert_eq!(
                        Some(&*display),
                        analysis.display_value(r, f).as_deref(),
                        "{what}: display value"
                    );
                    assert_eq!(
                        parser.display_value_in(&bytes[base..], base, field),
                        display,
                        "{what}: display value in a window"
                    );
                    check_prefixes(parser, bytes, field, &display, &what);
                }
            }
        }
    }
});

/// A display prefix is the full value's first `max` characters, and says
/// whether it was cut.
fn check_prefixes(parser: RowParser, bytes: &[u8], field: &FieldSpan, full: &str, what: &str) {
    let n = full.chars().count();
    for max in [0, 1, 2, 3, 7, n.saturating_sub(1), n, n + 1] {
        let (prefix, truncated) = parser.display_prefix(bytes, field, max);
        let expected: String = full.chars().take(max).collect();
        assert_eq!(prefix, expected, "{what}: prefix of {max}");
        assert_eq!(truncated, n > max, "{what}: prefix of {max} cut");
    }
}
