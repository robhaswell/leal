//! Tests for the row parser, display values and the row cache:
//! hand-written cases, the testkit's generated files (clean and messy,
//! UTF-8, Windows-1252 and UTF-16) and every corpus sidecar.
//!
//! The expected answers come from the testkit: the generator's layouts and
//! the corpus sidecars, both of which the testkit checks against its own
//! reference parser. That parser is never used here (0.2 notes).

use super::*;

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::Arc;

use leal_testkit::corpus::{self, CorpusCase};
use leal_testkit::diagnostics;
use leal_testkit::dialect::{
    Delimiter, Encoding as TkEncoding, LineEnding as TkLineEnding, decode_value,
    decode_windows_1252,
};
use leal_testkit::layout::{FieldLayout, Layout, RowLayout};
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, GeneratedCsv, csv_file, csv_file_utf16};
use proptest::prelude::*;

use crate::index::LineEnding;

// ---------------------------------------------------------------------------
// Helpers

fn dialect(delimiter: u8, encoding: Encoding, bom_len: usize) -> IndexDialect {
    IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: encoding.code_unit(),
        bom_len,
    }
}

fn parser(delimiter: u8, encoding: Encoding, bom_len: usize) -> RowParser {
    RowParser::new(dialect(delimiter, encoding, bom_len), encoding).unwrap()
}

fn utf8() -> RowParser {
    parser(b',', Encoding::Utf8, 0)
}

fn from_tk(encoding: TkEncoding) -> Encoding {
    match encoding {
        TkEncoding::Utf8 => Encoding::Utf8,
        TkEncoding::Utf16Le => Encoding::Utf16Le,
        TkEncoding::Utf16Be => Encoding::Utf16Be,
        TkEncoding::Windows1252 => Encoding::Windows1252,
    }
}

fn to_tk(le: LineEnding) -> TkLineEnding {
    match le {
        LineEnding::Lf => TkLineEnding::Lf,
        LineEnding::Crlf => TkLineEnding::Crlf,
        LineEnding::Cr => TkLineEnding::Cr,
    }
}

/// Indexes `bytes` and parses every row.
fn parse_all(bytes: &[u8], parser: RowParser) -> (RowIndex, Vec<ParsedRow>) {
    let index = RowIndex::build(bytes, parser.dialect()).unwrap();
    let rows = (0..index.row_count())
        .map(|r| parser.parse_row(&index, r, bytes).unwrap())
        .collect();
    (index, rows)
}

/// The parse as a testkit [`Layout`]. Values are the bytes before
/// decoding, except in UTF-16, where the testkit keeps values as UTF-8
/// ([`FieldLayout::value`]), so they are the display value's bytes.
fn to_layout(bytes: &[u8], parser: RowParser) -> Layout {
    let (index, rows) = parse_all(bytes, parser);
    let utf16 = parser.dialect().code_unit != CodeUnit::Byte;
    Layout {
        bom_len: parser.dialect().bom_len,
        rows: rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                let span = index.row(r, bytes).unwrap();
                assert_eq!(row.span(), span.span, "row {r}'s span");
                RowLayout {
                    span: span.span,
                    line_ending: span.line_ending.map(to_tk),
                    fields: row
                        .fields()
                        .iter()
                        .map(|f| FieldLayout {
                            span: f.span(),
                            quoted: f.quoted(),
                            value: if utf16 {
                                parser.display_value(bytes, f).into_owned().into_bytes()
                            } else {
                                parser.value_bytes(bytes, f).into_owned()
                            },
                            text_after_quote: f.text_after_quote(),
                            unterminated: f.unterminated(),
                        })
                        .collect(),
                }
            })
            .collect(),
    }
}

/// `(raw text, quoted, display value)` for each field of a one-row,
/// comma-delimited UTF-8 file.
fn fields_of(text: &str) -> Vec<(String, bool, String)> {
    let bytes = text.as_bytes();
    let p = utf8();
    let row = p.parse(bytes, 0..bytes.len()).unwrap();
    row.fields()
        .iter()
        .map(|f| {
            (
                String::from_utf8(f.raw(bytes).to_vec()).unwrap(),
                f.quoted(),
                p.display_value(bytes, f).into_owned(),
            )
        })
        .collect()
}

/// Display values of a one-row, comma-delimited UTF-8 file.
fn values_of(text: &str) -> Vec<String> {
    fields_of(text).into_iter().map(|(_, _, v)| v).collect()
}

fn utf16(text: &str, little_endian: bool) -> Vec<u8> {
    let mut bytes = if little_endian {
        vec![0xFF, 0xFE]
    } else {
        vec![0xFE, 0xFF]
    };
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    bytes
}

fn utf16_parser(little_endian: bool) -> RowParser {
    let encoding = if little_endian {
        Encoding::Utf16Le
    } else {
        Encoding::Utf16Be
    };
    parser(b',', encoding, 2)
}

/// Display values of every row of a UTF-16 file.
fn utf16_values(bytes: &[u8], little_endian: bool) -> Vec<Vec<String>> {
    let p = utf16_parser(little_endian);
    let (_, rows) = parse_all(bytes, p);
    rows.iter()
        .map(|row| {
            row.fields()
                .iter()
                .map(|f| p.display_value(bytes, f).into_owned())
                .collect()
        })
        .collect()
}

/// Checks that `display_prefix` gives the first `max_chars` characters of
/// `display_value`, for a spread of `max_chars` around the value's length,
/// and says whether it cut anything.
fn check_prefixes(p: RowParser, bytes: &[u8], field: &FieldSpan) -> Result<(), TestCaseError> {
    let full = p.display_value(bytes, field);
    let n = full.chars().count();
    for max in [0, 1, 2, 3, 5, 8, 13, 21, n.saturating_sub(1), n, n + 1] {
        let (prefix, truncated) = p.display_prefix(bytes, field, max);
        let expected: String = full.chars().take(max).collect();
        prop_assert_eq!(&prefix, &expected, "max_chars {}", max);
        prop_assert_eq!(truncated, n > max, "max_chars {}", max);
        // It borrows whenever the whole value does.
        if matches!(full, Cow::Borrowed(_)) {
            prop_assert!(matches!(prefix, Cow::Borrowed(_)), "max_chars {}", max);
        }
    }
    Ok(())
}

/// Checks the parse of a generated file against the generator's layout:
/// every row's and field's span, quoting, text after the closing quote and
/// unterminated quote, every value before decoding (in UTF-16, after), and
/// every display value, and its prefixes.
fn check_generated(file: &GeneratedCsv) -> Result<(), TestCaseError> {
    let encoding = from_tk(file.encoding);
    let p = parser(file.delimiter().byte(), encoding, file.layout.bom_len);
    let layout = to_layout(&file.bytes, p);
    prop_assert_eq!(&layout, &file.layout);
    let (_, rows) = parse_all(&file.bytes, p);
    for (r, row) in rows.iter().enumerate() {
        for (f, field) in row.fields().iter().enumerate() {
            let expected = decode_value(&file.layout.rows[r].fields[f].value, file.encoding);
            prop_assert_eq!(
                p.display_value(&file.bytes, field),
                expected,
                "row {} field {}",
                r,
                f
            );
            check_prefixes(p, &file.bytes, field)?;
        }
    }
    Ok(())
}

/// Messy files with long values and fewer, narrower rows (to keep each case
/// small).
fn long_values() -> CsvConfig {
    CsvConfig {
        max_rows: 4,
        max_fields: 3,
        max_value_chunks: 100,
        ..CsvConfig::messy()
    }
}

/// Checks that a row's fields tile it: each field starts where the last
/// one ended plus one delimiter unit, and the last ends at the row's end.
fn check_fields_tile(bytes: &[u8], p: RowParser, row: &ParsedRow) -> Result<(), String> {
    let w = p.dialect().code_unit.width();
    let delimiter = match p.dialect().code_unit {
        CodeUnit::Byte => vec![p.dialect().delimiter],
        CodeUnit::Utf16Le => vec![p.dialect().delimiter, 0],
        CodeUnit::Utf16Be => vec![0, p.dialect().delimiter],
    };
    let mut pos = row.span().start;
    for (i, f) in row.fields().iter().enumerate() {
        if i > 0 {
            if bytes.get(pos..pos + w) != Some(&delimiter[..]) {
                return Err(format!("no delimiter before field {i} at {pos}"));
            }
            pos += w;
        }
        if f.start() != pos {
            return Err(format!("field {i} starts at {}, expected {pos}", f.start()));
        }
        pos = f.span().end;
    }
    if pos != row.span().end {
        return Err(format!("fields end at {pos}, row at {}", row.span().end));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fields: hand-written cases

#[test]
fn plain_fields() {
    assert_eq!(
        fields_of("a,bc,,d"),
        vec![
            ("a".into(), false, "a".into()),
            ("bc".into(), false, "bc".into()),
            (String::new(), false, String::new()),
            ("d".into(), false, "d".into()),
        ]
    );
    let bytes = b"a,bc,,d";
    let row = utf8().parse(bytes, 0..7).unwrap();
    let spans: Vec<_> = row.fields().iter().map(FieldSpan::span).collect();
    assert_eq!(spans, vec![0..1, 2..4, 5..5, 6..7]);
    assert_eq!(row.field(3).map(FieldSpan::len), Some(1));
    assert_eq!(row.field(4), None);
}

#[test]
fn leading_and_trailing_delimiters_give_empty_fields() {
    assert_eq!(values_of(",a,"), vec!["", "a", ""]);
    assert_eq!(values_of(","), vec!["", ""]);
}

#[test]
fn a_blank_row_has_one_empty_unquoted_field() {
    let bytes = b"a\n\nb\n";
    let (_, rows) = parse_all(bytes, utf8());
    assert_eq!(rows.len(), 3);
    let blank = &rows[1];
    assert_eq!(blank.span(), 2..2);
    assert_eq!(blank.fields().len(), 1);
    let field = blank.fields()[0];
    assert_eq!(field.span(), 2..2);
    assert!(field.is_empty());
    assert!(!field.quoted());
}

#[test]
fn quoted_fields_unquote_and_unescape() {
    assert_eq!(
        fields_of(r#""a,b","x""y","""","""#),
        vec![
            (r#""a,b""#.into(), true, "a,b".into()),
            (r#""x""y""#.into(), true, r#"x"y"#.into()),
            (r#""""""#.into(), true, r#"""#.into()),
            (r#""""#.into(), true, String::new()),
        ]
    );
}

#[test]
fn quoted_newlines_and_delimiters_stay_in_the_field() {
    let bytes = b"h\n\"a\r\nb\",\"c\rd\",\"e\nf\"\nz\n";
    let (_, rows) = parse_all(bytes, utf8());
    assert_eq!(rows.len(), 3);
    let p = utf8();
    let values: Vec<_> = rows[1]
        .fields()
        .iter()
        .map(|f| p.display_value(bytes, f))
        .collect();
    assert_eq!(values, vec!["a\r\nb", "c\rd", "e\nf"]);
}

#[test]
fn a_quote_inside_an_unquoted_field_is_literal() {
    assert_eq!(
        fields_of(r#"a"b,c""#),
        vec![
            (r#"a"b"#.into(), false, r#"a"b"#.into()),
            (r#"c""#.into(), false, r#"c""#.into()),
        ]
    );
    // A space before the quote makes it literal too.
    assert_eq!(values_of(r#" "a",b"#), vec![r#" "a""#, "b"]);
}

#[test]
fn text_after_a_closing_quote_displays_raw() {
    // ADR-0003 decision 2: `"a"b` shows as `"a"b`.
    let bytes = br#""a"b,"Ada"x,"Bob" ,c"#;
    let row = utf8().parse(bytes, 0..bytes.len()).unwrap();
    let f = row.fields();
    assert_eq!(f.len(), 4);
    assert_eq!(f[0].span(), 0..4);
    assert!(f[0].quoted());
    assert_eq!(f[0].text_after_quote(), Some(3));
    assert_eq!(utf8().display_value(bytes, &f[0]), r#""a"b"#);
    assert_eq!(f[1].text_after_quote(), Some(10));
    assert_eq!(utf8().display_value(bytes, &f[1]), r#""Ada"x"#);
    assert_eq!(utf8().display_value(bytes, &f[2]), r#""Bob" "#);
    assert_eq!(f[3].text_after_quote(), None);
    assert_eq!(utf8().display_value(bytes, &f[3]), "c");
}

#[test]
fn quotes_in_text_after_a_closing_quote_are_literal() {
    // ADR-0003 decision 3: `"a"b"c",` is one field.
    assert_eq!(
        fields_of(r#""a"b"c",d"#),
        vec![
            (r#""a"b"c""#.into(), true, r#""a"b"c""#.into()),
            ("d".into(), false, "d".into()),
        ]
    );
    // The literal quote doesn't hide the delimiter after it, or a newline.
    let bytes = b"\"a\"b\",x\n\"c\"\"\"d\"\ny\n";
    let (_, rows) = parse_all(bytes, utf8());
    assert_eq!(rows.len(), 3);
    let p = utf8();
    let values: Vec<_> = rows
        .iter()
        .map(|r| {
            r.fields()
                .iter()
                .map(|f| p.display_value(bytes, f).into_owned())
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        values,
        vec![
            vec![r#""a"b""#.to_owned(), "x".to_owned()],
            vec![r#""c"""d""#.to_owned()],
            vec!["y".to_owned()],
        ]
    );
    assert_eq!(rows[1].fields()[0].text_after_quote(), Some(13));
}

#[test]
fn an_unterminated_quote_runs_to_the_end_of_the_file() {
    let bytes = b"id,name\n1,\"never closed\n3,lost\n";
    let (index, rows) = parse_all(bytes, utf8());
    assert_eq!(index.unterminated_quote(), Some(10));
    let last = &rows[1];
    assert_eq!(last.fields().len(), 2);
    let f = last.fields()[1];
    assert!(f.quoted());
    assert!(f.unterminated());
    assert_eq!(f.text_after_quote(), None);
    assert_eq!(f.span(), 10..bytes.len());
    assert_eq!(utf8().display_value(bytes, &f), "never closed\n3,lost\n");

    // Escaped quotes inside it are unescaped.
    assert_eq!(values_of(r#"a,"b""c"#), vec!["a", r#"b"c"#]);
    assert_eq!(values_of(r#"a,"b"""#), vec!["a", r#"b""#]);
    // A quote as the very last byte.
    let fields = fields_of(r#"a,""#);
    assert_eq!(fields[1], (r#"""#.into(), true, String::new()));
    let row = utf8().parse(br#"a,""#, 0..3).unwrap();
    assert!(row.fields()[1].unterminated());
    // Only the last field of the last row can be unterminated.
    let row = utf8().parse(br#""a,b"#, 0..4).unwrap();
    assert_eq!(row.fields().len(), 1);
    assert!(row.fields()[0].unterminated());
}

#[test]
fn every_delimiter_splits_only_on_itself() {
    let bytes = b"a,b;c\td|e";
    for (delimiter, expected) in [
        (b',', vec!["a", "b;c\td|e"]),
        (b';', vec!["a,b", "c\td|e"]),
        (b'\t', vec!["a,b;c", "d|e"]),
        (b'|', vec!["a,b;c\td", "e"]),
    ] {
        let p = parser(delimiter, Encoding::Utf8, 0);
        let row = p.parse(bytes, 0..bytes.len()).unwrap();
        let values: Vec<_> = row
            .fields()
            .iter()
            .map(|f| p.display_value(bytes, f))
            .collect();
        assert_eq!(values, expected, "delimiter {:?}", char::from(delimiter));
    }
}

#[test]
fn a_quote_after_the_bom_opens_the_first_field() {
    let bytes = b"\xEF\xBB\xBF\"a,b\",c\n";
    let p = parser(b',', Encoding::Utf8, 3);
    let (_, rows) = parse_all(bytes, p);
    let f = rows[0].fields();
    assert_eq!(f[0].span(), 3..8);
    assert!(f[0].quoted());
    assert_eq!(p.display_value(bytes, &f[0]), "a,b");

    // Read as Windows-1252 with no BOM, the BOM bytes are text, and the
    // quote after them is literal.
    let p = parser(b',', Encoding::Windows1252, 0);
    let (_, rows) = parse_all(bytes, p);
    let values: Vec<_> = rows[0]
        .fields()
        .iter()
        .map(|f| p.display_value(bytes, f))
        .collect();
    assert_eq!(values, vec!["ï»¿\"a", "b\"", "c"]);
}

#[test]
fn a_nul_quote_character_works_like_any_other() {
    // The parser accepts any ASCII quote other than CR, LF and the
    // delimiter, NUL included. The `""` search pads short blocks with NUL
    // bytes, which must not count as quotes.
    let p = RowParser::new(
        IndexDialect {
            quote: 0,
            ..dialect(b',', Encoding::Utf8, 0)
        },
        Encoding::Utf8,
    )
    .unwrap();
    let values = |bytes: &[u8]| -> Vec<(String, bool, bool)> {
        let (_, rows) = parse_all(bytes, p);
        rows.iter()
            .flat_map(|r| r.fields().to_vec())
            .map(|f| {
                let text = p.display_value(bytes, &f).into_owned();
                (text, f.quoted(), f.unterminated())
            })
            .collect()
    };
    // Closed, with an escaped quote: `\0x\0\0y\0` is `x\0y`.
    assert_eq!(
        values(b"\0x\0\0y\0,b"),
        vec![("x\0y".into(), true, false), ("b".into(), false, false)]
    );
    // Unterminated after a `\0\0`, less than 64 bytes from it to the end.
    assert_eq!(
        values(b"a,\0x\0\0yz"),
        vec![("a".into(), false, false), ("x\0yz".into(), true, true)]
    );
    let (_, rows) = parse_all(b"a,\0x\0\0yz", p);
    assert_eq!(rows[0].fields()[1].span(), 2..8);
    assert_eq!(rows[0].fields()[1].text_after_quote(), None);
}

#[test]
fn spans_outside_the_file_give_none() {
    let p = utf8();
    let bytes = b"a,b";
    assert_eq!(p.parse(bytes, 0..4), None);
    assert_eq!(p.parse(bytes, 4..4), None);
    #[allow(clippy::reversed_empty_ranges)]
    let backwards = 2..1;
    assert_eq!(p.parse(bytes, backwards), None);
    // Inside the BOM.
    let p = parser(b',', Encoding::Utf8, 3);
    assert_eq!(p.parse(b"\xEF\xBB\xBFa", 1..4), None);
    assert!(p.parse(b"\xEF\xBB\xBFa", 3..4).is_some());
    // An empty span at the end is a blank row.
    let row = utf8().parse(bytes, 3..3).unwrap();
    assert_eq!(row.fields().len(), 1);
}

#[test]
fn utf16_spans_must_be_whole_code_units() {
    let mut bytes = utf16("a,b", true);
    let p = utf16_parser(true);
    assert!(p.parse(&bytes, 2..8).is_some());
    assert_eq!(p.parse(&bytes, 3..8), None);
    assert_eq!(p.parse(&bytes, 2..7), None);
    assert_eq!(p.parse(&bytes, 0..8), None);
    // A final odd byte at the end of the file is allowed.
    bytes.push(b'x');
    let row = p.parse(&bytes, 2..9).unwrap();
    assert_eq!(row.fields().last().map(FieldSpan::span), Some(6..9));
}

#[test]
fn parse_row_needs_the_indexed_row_and_dialect() {
    let bytes = b"a,b\nc;d\n";
    let index = RowIndex::build(bytes, dialect(b',', Encoding::Utf8, 0)).unwrap();
    assert!(utf8().parse_row(&index, 1, bytes).is_some());
    assert_eq!(utf8().parse_row(&index, 2, bytes), None);
    // The wrong bytes (a different length) or another dialect.
    assert_eq!(utf8().parse_row(&index, 0, b"a,b\n"), None);
    let semicolon = parser(b';', Encoding::Utf8, 0);
    assert_eq!(semicolon.parse_row(&index, 0, bytes), None);
}

#[test]
fn a_parser_needs_a_usable_dialect_and_a_matching_encoding() {
    let ok = dialect(b',', Encoding::Utf8, 0);
    for (delimiter, quote) in [(b'"', b'"'), (b'\n', b'"'), (b',', b'\r'), (0xE9, b'"')] {
        let d = IndexDialect {
            delimiter,
            quote,
            ..ok
        };
        assert_eq!(
            RowParser::new(d, Encoding::Utf8),
            Err(RowsError::InvalidDialect { delimiter, quote })
        );
    }
    assert_eq!(
        RowParser::new(ok, Encoding::Utf16Le),
        Err(RowsError::EncodingMismatch {
            code_unit: CodeUnit::Byte,
            encoding: Encoding::Utf16Le
        })
    );
    let utf16 = dialect(b',', Encoding::Utf16Be, 2);
    assert!(RowParser::new(utf16, Encoding::Utf16Be).is_ok());
    assert!(RowParser::new(utf16, Encoding::Utf16Le).is_err());
    assert!(RowParser::new(utf16, Encoding::Utf8).is_err());
    for encoding in Encoding::ALL {
        let d = dialect(b',', encoding, 0);
        let p = RowParser::new(d, encoding).unwrap();
        assert_eq!((p.dialect(), p.encoding()), (d, encoding));
    }
    // The errors say what is wrong.
    let err = RowParser::new(ok, Encoding::Utf16Le).unwrap_err();
    assert!(err.to_string().contains("Utf16Le"), "{err}");
}

// ---------------------------------------------------------------------------
// Display values: encodings

#[test]
fn invalid_utf8_displays_as_replacement_characters() {
    let bytes = b"caf\xC3\xA9,\xFF\xFEx,\"\xE2\x82\"\"\"";
    let p = utf8();
    let row = p.parse(bytes, 0..bytes.len()).unwrap();
    let values: Vec<_> = row
        .fields()
        .iter()
        .map(|f| p.display_value(bytes, f))
        .collect();
    assert_eq!(values, vec!["café", "\u{FFFD}\u{FFFD}x", "\u{FFFD}\""]);
}

#[test]
fn display_values_borrow_from_the_file_when_nothing_changes() {
    let bytes = "plain,\"quoted, é\",\"esc\"\"aped\",\"a\"b,bad\u{0}\u{FFFD}".as_bytes();
    let p = utf8();
    let row = p.parse(bytes, 0..bytes.len()).unwrap();
    let borrowed: Vec<bool> = row
        .fields()
        .iter()
        .map(|f| matches!(p.display_value(bytes, f), Cow::Borrowed(_)))
        .collect();
    // Only the field with `""` needs a new string.
    assert_eq!(borrowed, vec![true, true, false, true, true]);
    let invalid = b"\xFF";
    let row = p.parse(invalid, 0..1).unwrap();
    assert!(matches!(
        p.display_value(invalid, &row.fields()[0]),
        Cow::Owned(_)
    ));
    // ASCII in a single-byte encoding borrows too.
    let p = parser(b',', Encoding::Windows1252, 0);
    let row = p.parse(b"abc", 0..3).unwrap();
    assert!(matches!(
        p.display_value(b"abc", &row.fields()[0]),
        Cow::Borrowed("abc")
    ));
}

#[test]
fn windows_1252_matches_the_testkit_for_every_byte() {
    // Every byte but the structural ones, in one unquoted field.
    let bytes: Vec<u8> = (0..=255u8).filter(|b| !b",\"\r\n".contains(b)).collect();
    let p = parser(b',', Encoding::Windows1252, 0);
    let row = p.parse(&bytes, 0..bytes.len()).unwrap();
    assert_eq!(row.fields().len(), 1);
    assert_eq!(
        p.display_value(&bytes, &row.fields()[0]),
        decode_windows_1252(&bytes)
    );
}

#[test]
fn single_byte_encodings_decode_their_own_characters() {
    // Well-known characters from each code page.
    let cases: &[(Encoding, u8, char)] = &[
        (Encoding::Windows1252, 0x80, '€'),
        (Encoding::Windows1252, 0x81, '\u{81}'),
        (Encoding::Windows1250, 0x8A, 'Š'),
        (Encoding::Windows1250, 0xA5, 'Ą'),
        (Encoding::Windows1251, 0xC0, 'А'),
        (Encoding::Windows1251, 0xFF, 'я'),
        (Encoding::Windows1253, 0xC1, 'Α'),
        (Encoding::Windows1254, 0xD0, 'Ğ'),
        (Encoding::Windows1255, 0xE0, 'א'),
        (Encoding::Windows1256, 0xC7, 'ا'),
        (Encoding::Windows1257, 0xC0, 'Ą'),
        (Encoding::Windows1258, 0xD0, 'Đ'),
        (Encoding::Iso8859_1, 0x80, '\u{80}'),
        (Encoding::Iso8859_1, 0xA4, '¤'),
        (Encoding::Iso8859_1, 0xE9, 'é'),
        (Encoding::Iso8859_2, 0xA1, 'Ą'),
        (Encoding::Iso8859_15, 0xA4, '€'),
        (Encoding::MacRoman, 0x80, 'Ä'),
        (Encoding::MacRoman, 0x8E, 'é'),
    ];
    for &(encoding, byte, expected) in cases {
        let bytes = [b'x', byte, b',', b'"', byte, b'"'];
        let p = parser(b',', encoding, 0);
        let row = p.parse(&bytes, 0..bytes.len()).unwrap();
        let values: Vec<_> = row
            .fields()
            .iter()
            .map(|f| p.display_value(&bytes, f).into_owned())
            .collect();
        assert_eq!(
            values,
            vec![format!("x{expected}"), expected.to_string()],
            "{encoding:?} byte {byte:#04x}"
        );
    }
}

#[test]
fn unmapped_single_byte_bytes_display_as_replacement_characters() {
    // Windows-1253 leaves 0xAA unassigned.
    let p = parser(b',', Encoding::Windows1253, 0);
    let bytes = b"a\xAAb";
    let row = p.parse(bytes, 0..3).unwrap();
    assert_eq!(p.display_value(bytes, &row.fields()[0]), "a\u{FFFD}b");
}

#[test]
fn utf16_fields_are_byte_offsets_into_the_file() {
    for little_endian in [true, false] {
        let bytes = utf16("id,\"x,\"\"y\"\"\"\n1,é😀\n", little_endian);
        let p = utf16_parser(little_endian);
        let (_, rows) = parse_all(&bytes, p);
        assert_eq!(rows.len(), 2);
        let spans: Vec<_> = rows[0].fields().iter().map(FieldSpan::span).collect();
        // BOM (2) + "id" (4), then "," (2), then the quoted field.
        assert_eq!(spans, vec![2..6, 8..26]);
        assert!(rows[0].fields()[1].quoted());
        let spans: Vec<_> = rows[1].fields().iter().map(FieldSpan::span).collect();
        assert_eq!(spans, vec![28..30, 32..38]);
        assert_eq!(
            utf16_values(&bytes, little_endian),
            vec![vec!["id", "x,\"y\""], vec!["1", "é😀"]],
            "little-endian: {little_endian}"
        );
    }
}

#[test]
fn utf16_units_whose_other_byte_is_structural_are_text() {
    // U+222C and U+2C22 hold 0x2C (`,`) and 0x22 (`"`) in either byte.
    for little_endian in [true, false] {
        let bytes = utf16("\u{222C}\u{2C22},\u{2C22}a\u{222C}\n", little_endian);
        assert_eq!(
            utf16_values(&bytes, little_endian),
            vec![vec!["\u{222C}\u{2C22}", "\u{2C22}a\u{222C}"]]
        );
        let p = utf16_parser(little_endian);
        let (_, rows) = parse_all(&bytes, p);
        assert!(!rows[0].fields()[1].quoted());
    }
}

#[test]
fn utf16_text_after_quote_and_unterminated_quotes() {
    for little_endian in [true, false] {
        let bytes = utf16("\"a\"b\"c\",\"d\n\"\"e", little_endian);
        let p = utf16_parser(little_endian);
        let (_, rows) = parse_all(&bytes, p);
        assert_eq!(rows.len(), 1);
        let f = rows[0].fields();
        assert_eq!(f[0].text_after_quote(), Some(2 + 3 * 2));
        assert!(f[1].unterminated());
        assert_eq!(f[1].span(), 18..bytes.len());
        assert_eq!(
            utf16_values(&bytes, little_endian),
            vec![vec!["\"a\"b\"c\"", "d\n\"e"]]
        );
    }
}

#[test]
fn utf16_unpaired_surrogates_and_a_final_odd_byte_display_as_replacement_characters() {
    // ADR-0003 decision 7: a lone high, a lone low, a lone high at the
    // field's end, and a valid pair.
    let units: [u16; 9] = [
        u16::from(b'x'),
        0xD800,
        u16::from(b'y'),
        0xDC00,
        u16::from(b','),
        0xD83D,
        0xDE00,
        u16::from(b','),
        0xDBFF,
    ];
    for little_endian in [true, false] {
        let mut bytes = utf16("", little_endian);
        for u in units {
            bytes.extend_from_slice(&if little_endian {
                u.to_le_bytes()
            } else {
                u.to_be_bytes()
            });
        }
        assert_eq!(
            utf16_values(&bytes, little_endian),
            vec![vec!["x\u{FFFD}y\u{FFFD}", "😀", "\u{FFFD}"]]
        );
        // A final odd byte belongs to the last field and is invalid.
        bytes.push(b'z');
        assert_eq!(
            utf16_values(&bytes, little_endian),
            vec![vec!["x\u{FFFD}y\u{FFFD}", "😀", "\u{FFFD}\u{FFFD}"]]
        );
        let p = utf16_parser(little_endian);
        let (_, rows) = parse_all(&bytes, p);
        assert_eq!(rows[0].fields()[2].span().end, bytes.len());
    }
}

#[test]
fn utf16_nul_units_are_kept() {
    let bytes = utf16("a\u{0}b,\u{0}", true);
    assert_eq!(utf16_values(&bytes, true), vec![vec!["a\u{0}b", "\u{0}"]]);
}

#[test]
fn raw_bytes_of_the_wrong_file_are_empty_not_a_panic() {
    let bytes = b"\"abc\"\"d\",e";
    let p = utf8();
    let row = p.parse(bytes, 0..bytes.len()).unwrap();
    for f in row.fields() {
        assert_eq!(f.raw(b"x"), b"");
        assert_eq!(p.display_value(b"x", f), "");
        assert_eq!(p.value_bytes(b"x", f), Cow::Borrowed(&b""[..]));
    }
}

// ---------------------------------------------------------------------------
// Generated files and the corpus

proptest! {
    #[test]
    fn clean_files_match_the_model(file in csv_file(CsvConfig::clean())) {
        check_generated(&file)?;
    }

    #[test]
    fn messy_files_match_the_model(file in csv_file(CsvConfig::messy())) {
        check_generated(&file)?;
    }

    #[test]
    fn clean_utf16_files_match_the_model(file in csv_file_utf16(CsvConfig::clean())) {
        check_generated(&file)?;
    }

    #[test]
    fn messy_utf16_files_match_the_model(file in csv_file_utf16(CsvConfig::messy())) {
        check_generated(&file)?;
    }

    /// Values of up to 100 chunks (up to 500 bytes), so quoted fields reach
    /// past the 64-byte blocks of the `""` search and the windows of
    /// `display_prefix`; the default values are at most 20 bytes.
    #[test]
    fn messy_files_with_long_values_match_the_model(file in csv_file(long_values())) {
        check_generated(&file)?;
    }

    #[test]
    fn messy_utf16_files_with_long_values_match_the_model(
        file in csv_file_utf16(long_values())
    ) {
        check_generated(&file)?;
    }

    /// Any bytes, in any delimiter and every encoding: parsing and decoding
    /// never panic, and fields tile every row.
    #[test]
    fn any_bytes_parse_into_fields_that_tile_each_row(
        bytes in csv_bytes(),
        delimiter in prop::sample::select(&Delimiter::ALL[..]),
        encoding in prop::sample::select(&Encoding::ALL[..]),
    ) {
        let bom_len = if encoding.code_unit() == CodeUnit::Byte { 0 } else { 2 };
        let mut file = match encoding {
            Encoding::Utf16Le => vec![0xFF, 0xFE],
            Encoding::Utf16Be => vec![0xFE, 0xFF],
            _ => Vec::new(),
        };
        file.extend_from_slice(&bytes);
        let p = parser(delimiter.byte(), encoding, bom_len);
        let (_, rows) = parse_all(&file, p);
        for row in &rows {
            check_fields_tile(&file, p, row).map_err(TestCaseError::fail)?;
            for f in row.fields() {
                let shown = p.display_value(&file, f);
                if encoding == Encoding::Utf8 {
                    let value = p.value_bytes(&file, f);
                    prop_assert_eq!(shown, String::from_utf8_lossy(&value));
                }
                check_prefixes(p, &file, f)?;
            }
        }
        if encoding.code_unit() == CodeUnit::Byte {
            let layout = to_layout(&file, p);
            prop_assert_eq!(layout.check_tiles(&file, delimiter), Ok(()));
        }
    }
}

/// Builds one field from arbitrary content, in `encoding`: quoted (every
/// quote doubled) or unquoted (delimiters, quotes, CR and LF dropped). In
/// UTF-16 the content's bytes are taken in pairs as code units, so it
/// holds unpaired surrogates and pairs as well as text.
fn one_field(content: &[u8], encoding: Encoding, quoted: bool) -> Vec<u8> {
    let units: Vec<u16> = match encoding.code_unit() {
        CodeUnit::Byte => content.iter().map(|&b| u16::from(b)).collect(),
        _ => content
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .collect(),
    };
    let structural = |u: u16| (*b",\"\r\n").map(u16::from).contains(&u);
    let mut field: Vec<u16> = Vec::new();
    if quoted {
        field.push(u16::from(b'"'));
        for &u in &units {
            field.push(u);
            if u == u16::from(b'"') {
                field.push(u);
            }
        }
        field.push(u16::from(b'"'));
    } else {
        field.extend(units.iter().copied().filter(|&u| !structural(u)));
    }
    match encoding {
        Encoding::Utf16Le => field.iter().flat_map(|u| u.to_le_bytes()).collect(),
        Encoding::Utf16Be => field.iter().flat_map(|u| u.to_be_bytes()).collect(),
        // Every unit came from one byte.
        _ => field.iter().map(|&u| u8::try_from(u).unwrap()).collect(),
    }
}

/// What long fields are made of: ASCII, quotes, structural bytes, UTF-8
/// characters of 2, 3 and 4 bytes, a truncated sequence, an invalid byte,
/// NUL, and (read as UTF-16 LE units) a lone high and low surrogate and a
/// pair.
const PIECES: &[&[u8]] = &[
    b"a",
    b"\"",
    b"\"\"",
    b",",
    b"\r\n",
    b"\xC3\xA9",
    b"\xE6\x9D\xB1",
    b"\xF0\x9F\x98\x80",
    b"\xE2\x82",
    b"\xFF",
    b"\x00",
    b"\x3D\xD8",
    b"\x00\xDC",
    b"\x3D\xD8\x00\xDE",
];

proptest! {
    /// Long fields full of the awkward cases (`""`, multibyte and invalid
    /// UTF-8, surrogates, single-byte text), so the prefix's window is cut
    /// in the middle of each of them: the prefix is always the start of
    /// the display value.
    #[test]
    fn prefixes_of_long_fields_are_the_start_of_the_display_value(
        content in prop::collection::vec(
            prop::sample::select(PIECES),
            0..200,
        ),
        encoding in prop::sample::select(&Encoding::ALL[..]),
        quoted in any::<bool>(),
        max in 0usize..64,
    ) {
        let content: Vec<u8> = content.concat();
        let bom = match encoding {
            Encoding::Utf16Le => &[0xFF, 0xFE][..],
            Encoding::Utf16Be => &[0xFE, 0xFF][..],
            _ => &[][..],
        };
        let mut bytes = bom.to_vec();
        bytes.extend(one_field(&content, encoding, quoted));
        let p = parser(b',', encoding, bom.len());
        let row = p.parse(&bytes, bom.len()..bytes.len()).unwrap();
        prop_assert_eq!(row.fields().len(), 1);
        let field = &row.fields()[0];
        // A quoted field closes at its last unit, however many `""` it has.
        prop_assert_eq!(field.quoted(), quoted);
        prop_assert_eq!(field.text_after_quote(), None);
        prop_assert!(!field.unterminated());
        let full = p.display_value(&bytes, field);
        let (prefix, truncated) = p.display_prefix(&bytes, field, max);
        let expected: String = full.chars().take(max).collect();
        prop_assert_eq!(prefix, expected);
        prop_assert_eq!(truncated, full.chars().count() > max);
    }
}

#[test]
fn a_prefix_of_a_megabyte_field_reads_only_its_start() {
    // 1 MB with `""` every few bytes, quoted; cut at 100 characters.
    let mut bytes = b"a,\"".to_vec();
    while bytes.len() < 1 << 20 {
        bytes.extend_from_slice("caf\u{e9} \"\"x\"\" 東京, ".as_bytes());
    }
    bytes.push(b'"');
    let p = utf8();
    let row = p.parse(&bytes, 0..bytes.len()).unwrap();
    let field = &row.fields()[1];
    let (prefix, truncated) = p.display_prefix(&bytes, field, 100);
    assert!(truncated);
    assert_eq!(prefix.chars().count(), 100);
    assert!(prefix.starts_with("caf\u{e9} \"x\" 東京, "));
    let full = p.display_value(&bytes, field);
    assert!(full.starts_with(&*prefix));
    // A short field isn't cut, and borrows.
    let (prefix, truncated) = p.display_prefix(&bytes, &row.fields()[0], 100);
    assert_eq!((prefix, truncated), (Cow::Borrowed("a"), false));
    // The wrong file gives an empty prefix.
    assert_eq!(
        p.display_prefix(b"x", field, 10),
        (Cow::Borrowed(""), false)
    );
}

/// The parse of a corpus file, in the sidecar's delimiter and encoding.
fn corpus_layout(case: &CorpusCase) -> (RowParser, Layout) {
    let d = &case.sidecar.dialect;
    let p = parser(d.delimiter.byte(), from_tk(d.encoding), d.bom.bytes().len());
    (p, to_layout(&case.bytes, p))
}

#[test]
fn every_corpus_sidecar_matches() {
    let cases = corpus::load().unwrap();
    assert!(cases.len() >= 42, "only {} corpus files", cases.len());
    let mut cells = 0;
    for case in &cases {
        let name = &case.name;
        let (p, layout) = corpus_layout(case);
        let sidecar = &case.sidecar;

        assert_eq!(layout.rows.len(), sidecar.rows.count, "{name}: rows");
        assert_eq!(
            layout.field_counts(),
            sidecar.rows.field_counts(),
            "{name}: field counts"
        );
        if p.dialect().code_unit == CodeUnit::Byte {
            assert_eq!(
                layout.check_tiles(&case.bytes, sidecar.dialect.delimiter),
                Ok(()),
                "{name}"
            );
        }
        let (_, rows) = parse_all(&case.bytes, p);
        for row in &rows {
            check_fields_tile(&case.bytes, p, row).unwrap_or_else(|e| panic!("{name}: {e}"));
        }

        // Every diagnostic kind the sidecar lists, derived from these
        // field spans, and no others: text after a closing quote and the
        // unterminated quote come straight from the parse, and the
        // field-level kinds (invalid encoding, NUL) depend on where each
        // field starts and ends.
        let derived = diagnostics::derive(&layout, &case.bytes, sidecar.dialect.encoding);
        let kinds = |ds: &[diagnostics::Diagnostic]| ds.iter().map(|d| d.kind).collect::<Vec<_>>();
        let mut expected_kinds = kinds(&sidecar.diagnostics);
        expected_kinds.sort();
        assert_eq!(kinds(&derived), expected_kinds, "{name}: diagnostic kinds");
        for want in &sidecar.diagnostics {
            let got = derived.iter().find(|d| d.kind == want.kind).unwrap();
            assert_eq!(got.count, want.count, "{name}: {:?} count", want.kind);
            assert!(
                got.first.starts_with(&want.first),
                "{name}: {:?} at {:?}, expected {:?}",
                want.kind,
                got.first,
                want.first
            );
        }

        for cell in &sidecar.cells {
            let field = rows[cell.row]
                .field(cell.field)
                .unwrap_or_else(|| panic!("{name}: no field {} in row {}", cell.field, cell.row));
            assert_eq!(
                p.display_value(&case.bytes, field),
                cell.value,
                "{name}: row {} field {}",
                cell.row,
                cell.field
            );
            if let Some(quoted) = cell.quoted {
                assert_eq!(
                    field.quoted(),
                    quoted,
                    "{name}: row {} field {} quoted",
                    cell.row,
                    cell.field
                );
            }
            cells += 1;
        }
    }
    assert!(cells >= 40, "only {cells} cells checked");
}

// ---------------------------------------------------------------------------
// The row cache

/// Rows of `fixture_fields(r)` fields each: 2 to 5.
fn cache_fixture(rows: usize) -> (Vec<u8>, RowIndex) {
    let mut bytes = Vec::new();
    for r in 0..rows {
        bytes.extend_from_slice(format!("{r},\"row {r}\"").as_bytes());
        for _ in 2..fixture_fields(r) {
            bytes.extend_from_slice(b",x");
        }
        bytes.push(b'\n');
    }
    let index = RowIndex::build(&bytes, utf8().dialect()).unwrap();
    (bytes, index)
}

fn fixture_fields(row: usize) -> usize {
    2 + row % 4
}

#[test]
fn the_cache_returns_the_parsed_row_and_keeps_it() {
    let (bytes, index) = cache_fixture(10);
    let mut cache = RowCache::new(utf8(), 4);
    assert_eq!(cache.capacity(), 4);
    assert!(cache.is_empty());
    let first = cache.row(&index, 3, &bytes).unwrap();
    assert_eq!(*first, utf8().parse_row(&index, 3, &bytes).unwrap());
    assert!(cache.contains(3));
    assert_eq!(cache.len(), 1);
    // A second read is the same parsed row, not a new parse.
    let again = cache.row(&index, 3, &bytes).unwrap();
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(cache.parser(), utf8());
}

#[test]
fn the_cache_evicts_the_least_recently_used_row() {
    let (bytes, index) = cache_fixture(10);
    let mut cache = RowCache::new(utf8(), 3);
    for r in [0, 1, 2] {
        cache.row(&index, r, &bytes).unwrap();
    }
    // Reading row 0 again makes row 1 the least recently used.
    cache.row(&index, 0, &bytes).unwrap();
    cache.row(&index, 3, &bytes).unwrap();
    assert_eq!(cache.len(), 3);
    assert!(cache.contains(0) && cache.contains(2) && cache.contains(3));
    assert!(!cache.contains(1));
    cache.row(&index, 4, &bytes).unwrap();
    assert!(!cache.contains(2));
    cache.clear();
    assert!(cache.is_empty());
    assert!(!cache.contains(0));
}

#[test]
fn rows_the_index_does_not_have_are_not_cached() {
    let (bytes, index) = cache_fixture(2);
    let mut cache = RowCache::new(utf8(), 3);
    assert_eq!(cache.row(&index, 2, &bytes), None);
    assert!(cache.is_empty());
    // An index built with another dialect gives nothing either.
    let semicolon = RowIndex::build(&bytes, parser(b';', Encoding::Utf8, 0).dialect()).unwrap();
    assert_eq!(cache.row(&semicolon, 0, &bytes), None);
}

#[test]
fn a_cache_always_holds_at_least_one_row() {
    let (bytes, index) = cache_fixture(3);
    let mut cache = RowCache::new(utf8(), 0);
    assert_eq!(cache.capacity(), 1);
    cache.row(&index, 0, &bytes).unwrap();
    cache.row(&index, 1, &bytes).unwrap();
    assert_eq!(cache.len(), 1);
    assert!(cache.contains(1));
    assert_eq!(DEFAULT_CACHE_ROWS, 256);
}

#[test]
fn the_cache_is_bounded_by_fields_too() {
    let (bytes, index) = cache_fixture(10);
    // Rows 0 to 3 have 2, 3, 4 and 5 fields.
    let mut cache = RowCache::with_limits(utf8(), 10, 9);
    assert_eq!((cache.capacity(), cache.max_fields()), (10, 9));
    cache.row(&index, 0, &bytes).unwrap();
    cache.row(&index, 1, &bytes).unwrap();
    cache.row(&index, 2, &bytes).unwrap();
    assert_eq!(cache.fields(), 9);
    // Five more fields: rows 0 and 1 go, oldest first, and row 2 stays.
    cache.row(&index, 3, &bytes).unwrap();
    assert!(!cache.contains(0) && !cache.contains(1));
    assert!(cache.contains(2) && cache.contains(3));
    assert_eq!(cache.fields(), 9);
    // A row with more fields than the whole cache may hold is parsed but
    // not kept, and nothing is evicted for it.
    let mut small = RowCache::with_limits(utf8(), 10, 4);
    small.row(&index, 0, &bytes).unwrap();
    let wide = small.row(&index, 3, &bytes).unwrap();
    assert_eq!(wide.fields().len(), 5);
    assert!(small.contains(0) && !small.contains(3));
    small.clear();
    assert_eq!((small.len(), small.fields()), (0, 0));
    assert_eq!(DEFAULT_CACHE_FIELDS, 100_000);
    assert_eq!(RowCache::new(utf8(), 3).max_fields(), DEFAULT_CACHE_FIELDS);
}

proptest! {
    /// Any sequence of reads gives the same rows as parsing directly, and
    /// the cache holds exactly the rows a simple LRU model with both limits
    /// says it should.
    #[test]
    fn the_cache_behaves_like_an_lru_list(
        capacity in 1usize..6,
        max_fields in 0usize..16,
        reads in prop::collection::vec(0usize..12, 0..60),
    ) {
        let (bytes, index) = cache_fixture(10);
        let mut cache = RowCache::with_limits(utf8(), capacity, max_fields);
        // Most recently used at the front.
        let mut model: VecDeque<usize> = VecDeque::new();
        let fields = |m: &VecDeque<usize>| m.iter().map(|&r| fixture_fields(r)).sum::<usize>();
        for &r in &reads {
            let got = cache.row(&index, r, &bytes);
            let expected = utf8().parse_row(&index, r, &bytes);
            prop_assert_eq!(got.as_deref(), expected.as_ref());
            if model.contains(&r) {
                model.retain(|&m| m != r);
                model.push_front(r);
            } else if r < 10 && fixture_fields(r) <= max_fields {
                while model.len() >= capacity || fields(&model) + fixture_fields(r) > max_fields {
                    model.pop_back();
                }
                model.push_front(r);
            }
            prop_assert_eq!(cache.len(), model.len());
            prop_assert_eq!(cache.fields(), fields(&model));
            for m in 0..12 {
                prop_assert_eq!(cache.contains(m), model.contains(&m), "row {}", m);
            }
        }
    }
}
