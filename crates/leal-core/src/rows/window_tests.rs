//! Tests for reading rows from a window of the file (task 1.3a):
//! [`RowParser::parse_in`], the `_in` display methods and
//! [`RowCache::row_in`]. A row read from a window that holds its extent
//! must be exactly the row read from the whole file: the same fields at
//! the same offsets, and the same values. The whole-file versions are
//! checked against the testkit in `tests.rs`.

use super::*;

use leal_testkit::dialect::Encoding as TkEncoding;
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, csv_file, csv_file_utf16};
use proptest::prelude::*;

fn parser(delimiter: u8, encoding: Encoding, bom_len: usize) -> RowParser {
    let dialect = IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: encoding.code_unit(),
        bom_len,
    };
    RowParser::new(dialect, encoding).unwrap()
}

/// Every row of `bytes`, read from a window of exactly its extent and from
/// a window of the rows around it, against the whole file.
fn check_windows(bytes: &[u8], parser: RowParser) -> Result<(), TestCaseError> {
    let index = RowIndex::build(bytes, parser.dialect()).unwrap();
    let rows = index.row_count();
    let mut cache = RowCache::new(parser, 4);
    for r in 0..rows {
        let expected = parser.parse_row(&index, r, bytes).unwrap();
        let extent = index.row_extent(r).unwrap();
        let screen = index
            .rows_extent(r.saturating_sub(2)..(r + 3).min(rows))
            .unwrap();
        for window in [extent.clone(), screen] {
            let (w, base) = (&bytes[window.clone()], window.start);
            let row = parser.parse_row_in(&index, r, w, base);
            prop_assert_eq!(row.as_ref(), Some(&expected), "row {}", r);
            for field in expected.fields() {
                prop_assert_eq!(
                    parser.value_bytes_in(w, base, field),
                    parser.value_bytes(bytes, field)
                );
                prop_assert_eq!(
                    parser.display_value_in(w, base, field),
                    parser.display_value(bytes, field)
                );
                for max in [0, 1, 3, 20] {
                    prop_assert_eq!(
                        parser.display_prefix_in(w, base, field, max),
                        parser.display_prefix(bytes, field, max)
                    );
                }
            }
        }
        let cached = cache.row_in(&index, r, &bytes[extent.clone()], extent.start);
        prop_assert_eq!(cached.as_deref(), Some(&expected));
        // A window that doesn't hold the row gives nothing.
        if !extent.is_empty() {
            let short = &bytes[extent.start..extent.end - 1];
            prop_assert_eq!(parser.parse_row_in(&index, r, short, extent.start), None);
        }
    }
    Ok(())
}

#[test]
fn a_row_from_its_window_matches_the_whole_file() {
    let bytes = b"id,name\n1,\"Smith, \"\"Jo\"\"\"\n2,\"a\"b\n3,\"open";
    check_windows(bytes, parser(b',', Encoding::Utf8, 0)).unwrap();
}

#[test]
fn utf16_rows_from_a_window_match_the_whole_file() {
    for (encoding, bom) in [
        (Encoding::Utf16Le, [0xFF, 0xFE]),
        (Encoding::Utf16Be, [0xFE, 0xFF]),
    ] {
        let mut bytes = bom.to_vec();
        for unit in "a,\"b\"\"\u{222C}\"\r\n\"x\"y,\u{2C22}\n".encode_utf16() {
            bytes.extend_from_slice(&match encoding {
                Encoding::Utf16Le => unit.to_le_bytes(),
                _ => unit.to_be_bytes(),
            });
        }
        check_windows(&bytes, parser(b',', encoding, 2)).unwrap();
        // With a final odd byte.
        bytes.push(b'z');
        check_windows(&bytes, parser(b',', encoding, 2)).unwrap();
    }
}

#[test]
fn fields_outside_the_window_are_empty() {
    let bytes = b"ab,\"c\"\"d\"\n";
    let p = parser(b',', Encoding::Utf8, 0);
    let row = p.parse(bytes, 0..9).unwrap();
    let field = &row.fields()[1];
    // A window that starts after the field, or ends inside it.
    assert_eq!(p.display_value_in(&bytes[4..], 4, field), "");
    assert_eq!(p.display_value_in(&bytes[..6], 0, field), "");
    assert_eq!(p.value_bytes_in(&bytes[..6], 0, field).as_ref(), b"");
    assert_eq!(
        p.display_prefix_in(&bytes[4..], 4, field, 5),
        ("".into(), false)
    );
    // A span outside the window isn't parsed.
    assert_eq!(p.parse_in(&bytes[3..], 3, 0..9), None);
    assert_eq!(p.parse_in(&bytes[..5], 0, 0..9), None);
    // Inside it, the offsets are the file's.
    let from_window = p.parse_in(&bytes[3..], 3, 3..9).unwrap();
    assert_eq!(from_window.fields()[0].span(), 3..9);
    assert_eq!(
        p.display_value_in(&bytes[3..], 3, &from_window.fields()[0]),
        "c\"d"
    );
}

#[test]
fn text_after_a_closing_quote_keeps_its_offset_in_the_file() {
    let bytes = b"x\n\"a\"b,c\n";
    let p = parser(b',', Encoding::Utf8, 0);
    let row = p.parse_in(&bytes[2..], 2, 2..8).unwrap();
    assert_eq!(row.fields()[0].text_after_quote(), Some(5));
    assert_eq!(row, p.parse(bytes, 2..8).unwrap());
}

proptest! {
    /// Generated clean and messy files, UTF-8 or Windows-1252.
    #[test]
    fn generated_rows_from_windows_match_the_whole_file(
        file in csv_file(CsvConfig::messy()),
    ) {
        let encoding = match file.encoding {
            TkEncoding::Windows1252 => Encoding::Windows1252,
            _ => Encoding::Utf8,
        };
        let p = parser(file.delimiter().byte(), encoding, file.layout.bom_len);
        check_windows(&file.bytes, p)?;
    }

    /// Generated UTF-16 files.
    #[test]
    fn generated_utf16_rows_from_windows_match_the_whole_file(
        file in csv_file_utf16(CsvConfig::messy()),
    ) {
        let encoding = match file.encoding {
            TkEncoding::Utf16Be => Encoding::Utf16Be,
            _ => Encoding::Utf16Le,
        };
        let p = parser(file.delimiter().byte(), encoding, file.layout.bom_len);
        check_windows(&file.bytes, p)?;
    }

    /// Any bytes, in any delimiter and every encoding.
    #[test]
    fn rows_of_any_bytes_from_windows_match_the_whole_file(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&b",;\t|"[..]),
        encoding in prop::sample::select(&Encoding::ALL[..]),
    ) {
        let mut file = match encoding {
            Encoding::Utf16Le => vec![0xFF, 0xFE],
            Encoding::Utf16Be => vec![0xFE, 0xFF],
            _ => Vec::new(),
        };
        let bom_len = file.len();
        file.extend_from_slice(&bytes);
        check_windows(&file, parser(delimiter, encoding, bom_len))?;
    }
}
