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
        source: Encoding::Utf8,
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

/// The splices of row 0 of `bytes` (a file in `source`, parsed with
/// `parser`), its first `span` bytes, with `cells` edited, written in
/// `encoding`.
fn splices_in(
    parser: &RowParser,
    bytes: &[u8],
    span: Range<usize>,
    cells: &[(usize, &str)],
    source: Encoding,
    encoding: Encoding,
) -> Result<(Vec<Splice>, Vec<Fix>), Vec<usize>> {
    let parsed = parser.parse(bytes, span).unwrap();
    let cells: Vec<(usize, Arc<str>)> = cells.iter().map(|&(c, v)| (c, Arc::from(v))).collect();
    let edited = EditedRow {
        row: 0,
        bytes,
        base: 0,
        span: parsed.span(),
        line_ending: Some(LineEnding::Lf),
        fields: parsed.fields(),
        cells: &cells,
        first_without_bom: parser.dialect().bom_len == 0,
    };
    let rules = RowRules {
        encoding,
        source,
        delimiter: b',',
        quote_all: false,
    };
    let (mut out, mut fixes) = (Vec::new(), Vec::new());
    row_splices(&edited, rules, &mut out, &mut fixes)?;
    Ok((out, fixes))
}

#[test]
fn a_value_that_cant_be_encoded_names_its_columns() {
    let w1252 = Encoding::Windows1252;
    let p = parser(b',');
    assert_eq!(
        splices_in(
            &p,
            b"a,b\n",
            0..3,
            &[(0, "😀"), (1, "x"), (2, "Ā")],
            w1252,
            w1252
        ),
        Err(vec![0, 2])
    );
    // A value it can hold is written in it.
    assert_eq!(
        splices_in(&p, b"a,b\n", 0..3, &[(0, "é€")], w1252, w1252),
        Ok((vec![splice(0..1, b"\xE9\x80")], vec![]))
    );
}

/// Save As UTF-8 from UTF-16: the edited field in UTF-8 over its UTF-16
/// bytes; a fix's whole row converted, its line ending's two units
/// included; and a field that can't be converted named.
#[test]
fn converting_rows_writes_utf8_over_the_files_bytes() {
    let utf16 = |text: &str| -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        bytes
    };
    let dialect = IndexDialect {
        delimiter: b',',
        quote: b'"',
        code_unit: CodeUnit::Utf16Le,
        bom_len: 2,
    };
    let p = RowParser::new(dialect, Encoding::Utf16Le).unwrap();
    let (le, utf8) = (Encoding::Utf16Le, Encoding::Utf8);
    let file = utf16("é,b\n");
    assert_eq!(
        splices_in(&p, &file, 2..8, &[(1, "😀")], le, utf8),
        Ok((vec![splice(6..8, "😀".as_bytes())], vec![]))
    );
    // A BOM: never BOM-like, so the edit is its field alone.
    assert_eq!(
        splices_in(&p, &file, 2..8, &[(0, "\u{FEFF}")], le, utf8),
        Ok((vec![splice(2..4, "\u{FEFF}".as_bytes())], vec![]))
    );
    // `""`: the whole row, its LF's two bytes included, in UTF-8.
    let file = utf16("a\n");
    assert_eq!(
        splices_in(&p, &file, 2..4, &[(0, "")], le, utf8),
        Ok((
            vec![splice(2..6, b"\"\"\n")],
            vec![Fix::EmptyRowQuoted { row: 0 }]
        ))
    );
    // An unpaired surrogate in an unedited field, met by the BOM-like fix
    // of a file without a BOM (Windows-1253's 0xAA, unassigned, here).
    let w1253 = Encoding::Windows1253;
    let single = parser(b',');
    assert_eq!(
        splices_in(&single, b"a,\xAA\n", 0..3, &[(0, "\u{FEFF}x")], w1253, utf8),
        Err(vec![1])
    );
    // Not met otherwise: its bytes are copied, and converted there.
    assert_eq!(
        splices_in(&single, b"a,\xAA\n", 0..3, &[(0, "Ω")], w1253, utf8),
        Ok((vec![splice(0..1, "Ω".as_bytes())], vec![]))
    );
}

/// [`encode`]: every single-byte encoding writes each character it reads
/// as the byte it came from, and nothing for a character it doesn't have.
#[test]
fn every_single_byte_encoding_writes_back_what_it_reads() {
    let mut unassigned = 0;
    for encoding in Encoding::ALL
        .into_iter()
        .filter(|e| e.is_ascii_compatible() && *e != Encoding::Utf8)
    {
        for byte in 0..=u8::MAX {
            // As display values read it (`rows::display`).
            let read = match encoding.whatwg() {
                Some(decoder) => decoder.decode_without_bom_handling(&[byte]).0.into_owned(),
                None => char::from(byte).to_string(),
            };
            if read == "\u{FFFD}" {
                unassigned += 1;
                assert_eq!(read, "\u{FFFD}", "{encoding:?} {byte:#04x}");
                assert!(encode(&read, encoding).is_err(), "{encoding:?} {byte:#04x}");
                continue;
            }
            assert_eq!(
                encode(&read, encoding).as_deref(),
                Ok(&[byte][..]),
                "{encoding:?} {byte:#04x} reads as {read:?}"
            );
        }
        let refused = encode("ok 😀", encoding).unwrap_err();
        assert_eq!((refused.encoding, refused.character), (encoding, '😀'));
        assert!(matches!(encode("ascii", encoding), Ok(Cow::Borrowed(_))));
    }
    assert!(unassigned > 0);
    assert_eq!(encode("é", Encoding::Utf8).as_deref(), Ok("é".as_bytes()));
    assert_eq!(
        encode("é", Encoding::Utf16Be).as_deref(),
        Ok(&[0x00, 0xE9][..])
    );
}

/// The transcoder: each piece of a stretch converted, a pair or a unit
/// split between pieces, and every sequence that isn't text reported at
/// its offset, with nothing written for it.
#[test]
fn the_transcoder_converts_and_reports_what_isnt_text() {
    let convert = |encoding, bytes: &[u8], cut: usize| {
        let mut transcoder = Transcoder::new(encoding).unwrap();
        let (mut out, mut bad) = (Vec::new(), Vec::new());
        transcoder.push(&bytes[..cut], 100, false, &mut out, &mut |at| bad.push(at));
        transcoder.push(&bytes[cut..], 100 + cut, true, &mut out, &mut |at| {
            bad.push(at)
        });
        (String::from_utf8(out).unwrap(), bad)
    };
    // 😀 is D83D DE00; "a" 0061.
    let le = b"a\0\x3D\xD8\x00\xDEb\0";
    for cut in 0..=le.len() {
        assert_eq!(
            convert(Encoding::Utf16Le, le, cut),
            ("a😀b".to_owned(), vec![])
        );
    }
    // A lone high surrogate, a lone low one, and a final odd byte.
    let broken = b"\x3D\xD8a\0\x00\xDEb\0c";
    for cut in 0..=broken.len() {
        assert_eq!(
            convert(Encoding::Utf16Le, broken, cut),
            ("ab".to_owned(), vec![100, 104, 108]),
            "cut at {cut}"
        );
    }
    let be = b"\xD8\x3D\x00a";
    assert_eq!(
        convert(Encoding::Utf16Be, be, 1),
        ("a".to_owned(), vec![100])
    );
    // Single bytes: Windows-1253 leaves 0xAA unassigned.
    let greek = b"\xC1\xAAb";
    for cut in 0..=greek.len() {
        assert_eq!(
            convert(Encoding::Windows1253, greek, cut),
            ("Αb".to_owned(), vec![101])
        );
    }
    assert!(Transcoder::new(Encoding::Utf8).is_none());
    assert_eq!(
        Transcoder::convert(b"\xE9t\xE9", Encoding::Iso8859_1).as_deref(),
        Some("été".as_bytes())
    );
    assert_eq!(Transcoder::convert(b"\xAA", Encoding::Windows1253), None);
    assert_eq!(
        Transcoder::convert(b"\xFF", Encoding::Utf8).as_deref(),
        Some(&b"\xFF"[..])
    );
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
        utf8: false,
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
