//! The splice rules on single rows, the size check and the attribute plan,
//! by hand. The property tests (`document/tests/saving/properties.rs`)
//! compare whole saves with the save oracle.

use super::*;
use crate::attributes::{Fingerprint, Interpretation};
use crate::detect::{Choices, DialectSource, EncodingSource, Hints, detect};
use crate::dialect::{Delimiter, LineEnding};
use crate::index::{CodeUnit, IndexDialect};
use crate::rows::RowParser;

fn parser(delimiter: u8) -> RowParser {
    let dialect = IndexDialect {
        delimiter,
        quote: b'"',
        code_unit: CodeUnit::Byte,
        bom_len: 0,
    };
    RowParser::new(dialect, Encoding::Utf8).unwrap()
}

/// The splices and fixes for `row` (its bytes without the line ending, at
/// offset `base`, ending with `ending`) with `cells` edited.
fn splices(
    row: &[u8],
    base: usize,
    ending: Option<LineEnding>,
    cells: &[(usize, &str)],
    quote_all: bool,
) -> Result<(Vec<Splice>, Vec<Fix>), Vec<usize>> {
    let mut bytes = vec![b'#'; base];
    bytes.extend_from_slice(row);
    bytes.extend_from_slice(ending.map_or(&b""[..], LineEnding::bytes));
    let parsed = parser(b',').parse(&bytes, base..base + row.len()).unwrap();
    let cells: Vec<(usize, Arc<str>)> = cells.iter().map(|&(c, v)| (c, Arc::from(v))).collect();
    let edited = EditedRow {
        row: 3,
        bytes: &bytes,
        base: 0,
        span: parsed.span(),
        line_ending: ending,
        fields: parsed.fields(),
        cells: &cells,
        first_without_bom: base == 0,
    };
    let rules = RowRules {
        encoding: Encoding::Utf8,
        delimiter: b',',
        quote_all,
    };
    let (mut out, mut fixes) = (Vec::new(), Vec::new());
    row_splices(&edited, rules, &mut out, &mut fixes)?;
    Ok((out, fixes))
}

fn splice(range: Range<usize>, bytes: &[u8]) -> Splice {
    Splice {
        range,
        bytes: bytes.to_vec(),
    }
}

#[test]
fn an_edited_field_is_one_splice_quoted_as_it_needs() {
    let lf = Some(LineEnding::Lf);
    assert_eq!(
        splices(b"a,b,c", 10, lf, &[(1, "x")], false),
        Ok((vec![splice(12..13, b"x")], vec![]))
    );
    assert_eq!(
        splices(b"a,\"b\",c", 10, lf, &[(1, "x"), (2, "y,z")], false),
        Ok((
            vec![splice(12..15, b"\"x\""), splice(16..17, b"\"y,z\"")],
            vec![]
        ))
    );
    assert_eq!(
        splices(b"a,b", 10, lf, &[(0, "say \"hi\"")], false),
        Ok((vec![splice(10..11, b"\"say \"\"hi\"\"\"")], vec![]))
    );
}

#[test]
fn hatched_cells_are_one_insert_before_the_line_ending() {
    let crlf = Some(LineEnding::Crlf);
    assert_eq!(
        splices(b"a", 10, crlf, &[(3, "z")], false),
        Ok((vec![splice(11..11, b",,,z")], vec![]))
    );
    // In a file that quotes every field, quoted; and a blank line's field.
    assert_eq!(
        splices(b"\"a\"", 10, crlf, &[(1, "z")], true),
        Ok((vec![splice(13..13, b",\"z\"")], vec![]))
    );
    assert_eq!(
        splices(b"", 10, crlf, &[(0, "z")], true),
        Ok((vec![splice(10..10, b"\"z\"")], vec![]))
    );
    // A field and a hatched cell of the same row: field first.
    assert_eq!(
        splices(b"a,b", 10, None, &[(1, "B"), (2, "C")], false),
        Ok((vec![splice(12..13, b"B"), splice(13..13, b",C")], vec![]))
    );
}

#[test]
fn the_fixes_replace_the_whole_row() {
    let lf = Some(LineEnding::Lf);
    assert_eq!(
        splices(b"a", 10, lf, &[(0, "")], false),
        Ok((
            vec![splice(10..12, b"\"\"\n")],
            vec![Fix::EmptyRowQuoted { row: 3 }]
        ))
    );
    // With no line ending, the last row too.
    assert_eq!(
        splices(b"a", 10, None, &[(0, "")], false),
        Ok((
            vec![splice(10..11, b"\"\"")],
            vec![Fix::EmptyRowQuoted { row: 3 }]
        ))
    );
    assert_eq!(
        splices(b"a,b", 0, lf, &[(0, "\u{FEFF}x")], false),
        Ok((
            vec![splice(0..4, "\"\u{FEFF}x\",b\n".as_bytes())],
            vec![Fix::BomLikeQuoted]
        ))
    );
    // Not past the first row, and not once quoted.
    assert_eq!(
        splices(b"a,b", 10, lf, &[(0, "\u{FEFF}x")], false),
        Ok((vec![splice(10..11, "\u{FEFF}x".as_bytes())], vec![]))
    );
    assert_eq!(
        splices(b"a,b", 0, lf, &[(0, "\u{FEFF},x")], false),
        Ok((vec![splice(0..1, "\"\u{FEFF},x\"".as_bytes())], vec![]))
    );
}

#[test]
fn a_value_that_cant_be_encoded_names_its_columns() {
    let bytes = b"a,b\n".to_vec();
    let parsed = parser(b',').parse(&bytes, 0..3).unwrap();
    let cells: Vec<(usize, Arc<str>)> = vec![(0, Arc::from("é")), (1, Arc::from("x"))];
    let edited = EditedRow {
        row: 0,
        bytes: &bytes,
        base: 0,
        span: parsed.span(),
        line_ending: Some(LineEnding::Lf),
        fields: parsed.fields(),
        cells: &cells,
        first_without_bom: true,
    };
    let rules = RowRules {
        encoding: Encoding::Windows1252,
        delimiter: b',',
        quote_all: false,
    };
    let (mut out, mut fixes) = (Vec::new(), Vec::new());
    assert_eq!(
        row_splices(&edited, rules, &mut out, &mut fixes),
        Err(vec![0])
    );
    assert!(out.is_empty() && fixes.is_empty());
}

#[test]
fn a_file_over_the_size_leal_reads_is_refused() {
    let max = u64::try_from(MAX_FILE_BYTES).unwrap();
    assert_eq!(checked_len(MAX_FILE_BYTES - 1, 1), Ok(max));
    assert_eq!(checked_len(MAX_FILE_BYTES, 1), Err(max + 1));
    assert_eq!(checked_len(MAX_FILE_BYTES + 10, -10), Ok(max));
    assert_eq!(checked_len(5, -2), Ok(3));
}

/// The plan for `file` read as `detection` describes, the file having had
/// `com.apple.TextEncoding` when opened or not.
fn plan(file: &[u8], detection: &crate::detect::Detection, had: bool) -> AttributePlan {
    let len = u64::try_from(file.len()).unwrap();
    let census = needs_census(detection, had).then(|| crate::detect::Census::of(file, false));
    AttributePlan::decide(&AttributeFacts {
        detection,
        had_text_encoding: had,
        head: file,
        len,
        census,
    })
}

fn guessed(file: &[u8]) -> crate::detect::Detection {
    let len = u64::try_from(file.len()).unwrap();
    detect(file, len, Hints::default(), Choices::default()).unwrap()
}

#[test]
fn nothing_is_recorded_when_a_reopen_guesses_the_same() {
    let file = b"id;name\n1;Ada\n";
    let plan = plan(file, &guessed(file), false);
    assert_eq!(plan.text_encoding, None);
    assert_eq!(plan.interpretation, None);
}

#[test]
fn the_encoding_is_recorded_when_a_reopen_would_guess_another() {
    // Read as Windows-1252, saved as ASCII: a reopen would guess UTF-8.
    let mut detection = guessed(b"a,caf\xE9\n");
    assert_eq!(detection.encoding, Encoding::Windows1252);
    let saved = b"a,cafe\n";
    assert_eq!(
        plan(saved, &detection, false).text_encoding,
        Some(Encoding::Windows1252)
    );
    // The attribute the file had is updated; the user's choice recorded.
    let utf8 = guessed(saved);
    assert_eq!(plan(saved, &utf8, true).text_encoding, Some(Encoding::Utf8));
    detection = utf8;
    detection.encoding_source = EncodingSource::User;
    assert_eq!(
        plan(saved, &detection, false).text_encoding,
        Some(Encoding::Utf8)
    );
}

#[test]
fn a_bom_decides_unless_the_file_had_the_attribute() {
    let file = b"\xEF\xBB\xBFa,b\n";
    let detection = guessed(file);
    assert_eq!(plan(file, &detection, false).text_encoding, None);
    assert_eq!(
        plan(file, &detection, true).text_encoding,
        Some(Encoding::Utf8)
    );
}

#[test]
fn the_interpretation_is_recorded_for_a_choice_or_a_different_guess() {
    let file = b"a;b\n1;2\n";
    let mut detection = guessed(file);
    detection.header_source = DialectSource::User;
    detection.header = true;
    let expected = Interpretation {
        delimiter: Some(Delimiter::Semicolon),
        header: Some(true),
        file: Some(Fingerprint::of(file)),
    };
    assert_eq!(plan(file, &detection, false).interpretation, Some(expected));
    // Read with commas (one column), which a reopen wouldn't guess.
    let mut commas = guessed(file);
    commas.delimiter = Delimiter::Comma;
    assert_eq!(
        plan(file, &commas, false)
            .interpretation
            .and_then(|i| i.delimiter),
        Some(Delimiter::Comma)
    );
}
