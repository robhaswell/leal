//! Detection with attribute values and user choices (ADR-0004 decision 11,
//! ADR-0005 decisions 1, 4 and 5), and first paint versus the whole file.

use leal_core::attributes::{Fingerprint, Interpretation, text_encoding_value};
mod common;

use common::{detect, review};
use leal_core::detect::{
    ChoiceError, Choices, Detection, DialectSource, EncodingSource, FIRST_PAINT_BYTES, Hints, Note,
};
use leal_core::dialect::{Bom, Delimiter, Encoding, LineEnding};

fn plain(bytes: &[u8]) -> Detection {
    detect(bytes, Hints::default(), Choices::default()).unwrap()
}

fn with_encoding_attribute(bytes: &[u8], value: &[u8]) -> Detection {
    let hints = Hints {
        text_encoding: Some(value),
        ..Hints::default()
    };
    detect(bytes, hints, Choices::default()).unwrap()
}

fn with_interpretation(bytes: &[u8], value: &[u8]) -> Detection {
    let hints = Hints {
        interpretation: Some(value),
        ..Hints::default()
    };
    detect(bytes, hints, Choices::default()).unwrap()
}

fn attr(e: Encoding) -> Vec<u8> {
    text_encoding_value(e).into_bytes()
}

// ---- com.apple.TextEncoding (ADR-0004 decision 11) ------------------------

/// The attribute beats a guess it contradicts, both ways round.
#[test]
fn an_encoding_attribute_that_contradicts_the_guess_wins() {
    // Valid UTF-8 ("é"), but the attribute says Windows-1252 ("Ã©").
    let utf8 = "name\ncafé\n".as_bytes();
    assert_eq!(plain(utf8).encoding, Encoding::Utf8);
    let d = with_encoding_attribute(utf8, &attr(Encoding::Windows1252));
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Windows1252, EncodingSource::Attribute)
    );
    assert_eq!(d.notes, []);

    // Pure ASCII guesses UTF-8; the attribute says Windows-1252.
    let d = with_encoding_attribute(b"a,b\n", &attr(Encoding::Windows1252));
    assert_eq!(d.encoding, Encoding::Windows1252);
}

/// A UTF-8 attribute is honoured even over invalid bytes; they get the
/// usual invalid-encoding warning (1.5), not a different encoding.
#[test]
fn a_utf8_attribute_is_honoured_over_invalid_bytes() {
    let latin1 = b"name\ncaf\xE9\n";
    assert_eq!(plain(latin1).encoding, Encoding::Windows1252);
    let d = with_encoding_attribute(latin1, b"utf-8;134217984");
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Utf8, EncodingSource::Attribute)
    );
    assert_eq!(d.notes, []);
    // Nothing but invalid bytes: still UTF-8.
    let d = with_encoding_attribute(b"\xFF\xFF\xFF\n", b"utf-8;134217984");
    assert_eq!(d.encoding, Encoding::Utf8);
    // And the review never second-guesses the attribute.
    assert_eq!(review(latin1, &d).encoding_suggestion, None);
}

#[test]
fn a_utf16_attribute_without_a_utf16_bom_is_ignored() {
    for number in [256u32, 268_435_712, 335_544_576] {
        let value = format!("utf-16;{number}");
        let d = with_encoding_attribute(b"a,b\n1,2\n", value.as_bytes());
        assert_eq!(
            (d.encoding, d.encoding_source),
            (Encoding::Utf8, EncodingSource::Guess)
        );
        assert_eq!(d.notes, [Note::TextEncodingUtf16WithoutBom]);
    }
}

#[test]
fn a_bom_beats_the_attribute() {
    let utf8_bom = b"\xEF\xBB\xBFa,b\n";
    let d = with_encoding_attribute(utf8_bom, &attr(Encoding::Windows1252));
    assert_eq!(
        (d.encoding, d.encoding_source, d.bom),
        (Encoding::Utf8, EncodingSource::Bom, Bom::Utf8)
    );
    let utf16 = b"\xFF\xFEa\0,\0b\0\n\0";
    let d = with_encoding_attribute(utf16, &attr(Encoding::Utf8));
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Utf16Le, EncodingSource::Bom)
    );
    let d = with_encoding_attribute(utf16, b"utf-16;256");
    assert_eq!((d.encoding, &d.notes[..]), (Encoding::Utf16Le, &[][..]));
}

/// The other single-byte encodings (ADR-0005 decision 5) are honoured
/// only if the bytes decode under them.
#[test]
fn another_single_byte_attribute_needs_the_bytes_to_decode() {
    // 0xE9 is "é" in Windows-1250 and "ι" in Windows-1253.
    let d = with_encoding_attribute(b"name\ncaf\xE9\n", &attr(Encoding::Windows1250));
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Windows1250, EncodingSource::Attribute)
    );
    // 0xAA is unassigned in Windows-1253, so the guess applies instead.
    let d = with_encoding_attribute(b"name\ncaf\xAA\n", &attr(Encoding::Windows1253));
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Windows1252, EncodingSource::Guess)
    );
    assert_eq!(
        d.notes,
        [Note::TextEncodingDoesNotDecode {
            encoding: Encoding::Windows1253
        }]
    );
    // Every one of them decodes plain ASCII.
    for e in Encoding::ALL
        .into_iter()
        .filter(|e| e.is_ascii_compatible())
    {
        let d = with_encoding_attribute(b"a;b\n", &attr(e));
        assert_eq!(
            (d.encoding, d.encoding_source),
            (e, EncodingSource::Attribute)
        );
    }
}

#[test]
fn unsupported_and_unreadable_attributes_are_ignored_with_a_note() {
    let d = with_encoding_attribute(b"a\n", b"shift_jis;2561");
    assert_eq!(d.encoding_source, EncodingSource::Guess);
    assert_eq!(
        d.notes,
        [Note::TextEncodingUnsupported {
            cf_string_encoding: 2561
        }]
    );
    let d = with_encoding_attribute(b"a\n", b"UTF-8");
    assert_eq!(d.encoding_source, EncodingSource::Guess);
    assert_eq!(d.notes, [Note::TextEncodingUnreadable]);
}

/// A non-UTF-8, non-Windows-1252 attribute is checked on the first 64 KB
/// at first paint; the review checks the rest and suggests the guess if a
/// later byte doesn't decode.
#[test]
fn the_review_checks_the_whole_file_decodes_under_the_attribute() {
    let mut file = ascii_rows(FIRST_PAINT_BYTES + 100);
    file.extend_from_slice(b"caf\xAA\n");
    let d = with_encoding_attribute(&file, &attr(Encoding::Windows1253));
    assert_eq!(d.encoding_source, EncodingSource::Attribute);
    let r = review(&file, &d);
    assert_eq!(r.encoding_suggestion, Some(Encoding::Windows1252));
}

/// The other half (p1-review tests-5): a file that decodes under the
/// attribute's encoding all the way through gets no suggestion, even with
/// bytes past the first 64 KB that UTF-8 or Windows-1252 would read
/// differently (ADR-0004 decision 11: the attribute isn't second-guessed).
#[test]
fn a_file_that_decodes_under_the_attribute_gets_no_suggestion() {
    let mut file = ascii_rows(FIRST_PAINT_BYTES + 100);
    // "ę" and "ł" in Windows-1250.
    file.extend_from_slice(b"99,\xEAl\xB3\n");
    for e in [Encoding::Windows1250, Encoding::Windows1253] {
        let d = with_encoding_attribute(&file, &attr(e));
        assert_eq!(
            (d.encoding, d.encoding_source),
            (e, EncodingSource::Attribute)
        );
        assert_eq!(review(&file, &d).encoding_suggestion, None, "{e:?}");
    }
}

// ---- first paint and the whole-file rule (ADR-0005 decision 4) ------------

/// Rows of `id,name` ASCII, at least `len` bytes long.
fn ascii_rows(len: usize) -> Vec<u8> {
    let mut out = b"id,name\n".to_vec();
    let mut i = 0;
    while out.len() < len {
        out.extend_from_slice(format!("{i},row {i}\n").as_bytes());
        i += 1;
    }
    out
}

#[test]
fn first_paint_guesses_from_64_kb_and_the_review_suggests_a_change() {
    // ASCII for the first 64 KB, then Windows-1252.
    let mut file = ascii_rows(FIRST_PAINT_BYTES + 10);
    file.extend_from_slice(b"99,caf\xE9\n");
    let d = plain(&file);
    assert_eq!(
        (d.encoding, d.encoding_source),
        (Encoding::Utf8, EncodingSource::Guess)
    );
    let r = review(&file, &d);
    assert_eq!(r.encoding_suggestion, Some(Encoding::Windows1252));
    // The document's encoding doesn't change by itself.
    assert_eq!(d.encoding, Encoding::Utf8);

    // The other way: one Windows-1252 byte early, lots of UTF-8 later.
    let mut file = b"id,name\n1,caf\xE9\n".to_vec();
    file.extend_from_slice(&ascii_rows(FIRST_PAINT_BYTES)[8..]);
    for i in 0..10 {
        file.extend_from_slice(format!("{i},café\n").as_bytes());
    }
    let d = plain(&file);
    assert_eq!(d.encoding, Encoding::Windows1252);
    assert_eq!(review(&file, &d).encoding_suggestion, Some(Encoding::Utf8));
}

#[test]
fn the_review_suggests_nothing_when_the_whole_file_agrees() {
    let file = ascii_rows(3 * FIRST_PAINT_BYTES);
    let d = plain(&file);
    let r = review(&file, &d);
    assert_eq!(
        (r.encoding_suggestion, r.delimiter_suggestion),
        (None, None)
    );
    assert_eq!(r.line_ending, Some(LineEnding::Lf));
    assert!(r.trailing_newline);
}

/// A UTF-8 character cut in half by the 64 KB limit is not an invalid
/// byte: the rest of it is just past the limit.
#[test]
fn a_character_cut_by_the_64_kb_limit_is_not_invalid() {
    for cut in 1..4 {
        // ASCII, then "😀" (4 bytes), of which `cut` fall inside the first
        // 64 KB. Counted as invalid, they would make the guess Windows-1252.
        let mut file = b"name\n".to_vec();
        file.resize(FIRST_PAINT_BYTES - cut, b'a');
        file.extend_from_slice("😀\n".as_bytes());
        assert_eq!(file[FIRST_PAINT_BYTES - cut], 0xF0);
        let d = plain(&file);
        assert_eq!(d.encoding, Encoding::Utf8, "cut {cut}");
    }
}

/// First paint reads only the first 64 KB: what comes after can't change
/// it, and it doesn't know the trailing newline of a larger file.
#[test]
fn first_paint_reads_only_the_first_64_kb() {
    let file = ascii_rows(FIRST_PAINT_BYTES + 1000);
    let d = plain(&file);
    assert_eq!(d.trailing_newline, None);
    let mut changed = file[..FIRST_PAINT_BYTES].to_vec();
    changed.extend_from_slice(b"\xFF;;;;\r\r\r\"");
    let e = plain(&changed);
    assert_eq!(e, d);
    // The review reads it all.
    assert!(review(&file, &d).trailing_newline);
    assert!(!review(&changed, &e).trailing_newline);
}

/// When most of the file, past the first 64 KB, fits a different
/// delimiter, the review suggests it; it never switches by itself.
#[test]
fn the_review_suggests_a_delimiter_the_whole_file_fits() {
    // The first 64 KB: one column, with one comma in the first row.
    let mut file = b"name,\n".to_vec();
    while file.len() < FIRST_PAINT_BYTES + 10 {
        file.extend_from_slice(b"x\n");
    }
    // The rest, three times as many rows: clearly semicolon-separated.
    for i in 0..100_000 {
        file.extend_from_slice(format!("{i};a;b\n").as_bytes());
    }
    let d = plain(&file);
    assert_eq!(d.delimiter, Delimiter::Comma);
    let r = review(&file, &d);
    assert_eq!(r.delimiter_suggestion, Some(Delimiter::Semicolon));
    // A delimiter the user chose is never second-guessed.
    let chosen = detect(
        &file,
        Hints::default(),
        Choices {
            delimiter: Some(Delimiter::Comma),
            ..Choices::default()
        },
    )
    .unwrap();
    assert_eq!(review(&file, &chosen).delimiter_suggestion, None);
}

#[test]
fn first_paint_drops_the_row_the_64_kb_limit_cuts() {
    // Every complete row has three fields and ends with CRLF. A first row
    // of `k` + 6 bytes moves where the limit cuts the 16-byte rows; with
    // k = 11 it falls between a CR and its LF.
    for k in [0, 1, 5, 11, 13, 15] {
        let mut file = vec![b'h'; k];
        file.extend_from_slice(b";b;c\r\n");
        while file.len() < FIRST_PAINT_BYTES * 2 {
            file.extend_from_slice(b"aaaa;bbbb;cccc\r\n");
        }
        let d = plain(&file);
        assert_eq!(d.delimiter, Delimiter::Semicolon, "{k}");
        assert_eq!(d.line_ending, Some(LineEnding::Crlf), "{k}");
        assert!(!d.mixed_line_endings, "{k}");
    }
    // The limit falls between CR and LF: that row is dropped too, rather
    // than counted as a lone CR.
    let mut file = vec![b'x'; FIRST_PAINT_BYTES - 1];
    file.extend_from_slice(b"\r\nmore\r\n");
    file[10] = b'\r';
    file[11] = b'\n';
    let d = plain(&file);
    assert!(!d.mixed_line_endings);
    assert_eq!(d.line_ending, Some(LineEnding::Crlf));
}

// ---- Leal's interpretation attribute (ADR-0005 decision 1) ----------------

#[test]
fn a_sensible_remembered_interpretation_is_honoured() {
    // One column whose values contain semicolons: the guess is ";", but the
    // file was saved as a one-column comma file. Even without a matching
    // fingerprint (something else changed the file), "," still fits.
    let bytes = b"a;b\nc;d\n";
    assert_eq!(plain(bytes).delimiter, Delimiter::Semicolon);
    let value = Interpretation {
        delimiter: Some(Delimiter::Comma),
        header: Some(false),
        file: Some(Fingerprint::of(b"a;b\n")),
        encoding: None,
    }
    .to_attribute_value();
    let d = with_interpretation(bytes, value.as_bytes());
    assert_eq!(
        (d.delimiter, d.delimiter_source, d.header, d.header_source),
        (
            Delimiter::Comma,
            DialectSource::Attribute,
            false,
            DialectSource::Attribute
        )
    );
    assert_eq!(d.notes, []);
}

#[test]
fn a_remembered_interpretation_that_no_longer_fits_is_ignored() {
    // Clearly semicolon-separated now; under "," the rows are ragged.
    let bytes = b"product;price;qty\nApple;1,20;3\nPear;0,95;12\nPlum;2,05;7\n";
    let d = with_interpretation(bytes, b"v=1;delimiter=comma;header=no");
    assert_eq!(
        (d.delimiter, d.delimiter_source, d.header, d.header_source),
        (
            Delimiter::Semicolon,
            DialectSource::Guess,
            true,
            DialectSource::Guess
        )
    );
    assert_eq!(
        d.notes,
        [Note::InterpretationNotSensible {
            delimiter: Delimiter::Comma
        }]
    );
}

/// While the file is the one Leal saved, the remembered delimiter is used
/// even where it fits worse than the guess.
#[test]
fn a_remembered_interpretation_is_honoured_while_the_file_is_unchanged() {
    let bytes = b"product;price;qty\nApple;1,20;3\nPear;0,95;12\nPlum;2,05;7\n";
    let value = Interpretation {
        delimiter: Some(Delimiter::Comma),
        header: Some(false),
        file: Some(Fingerprint::of(bytes)),
        encoding: None,
    }
    .to_attribute_value();
    let d = with_interpretation(bytes, value.as_bytes());
    assert_eq!(
        (d.delimiter, d.delimiter_source, d.header, d.notes),
        (Delimiter::Comma, DialectSource::Attribute, false, vec![])
    );
    // One byte more, and it is checked again.
    let mut changed = bytes.to_vec();
    changed.push(b'\n');
    let d = with_interpretation(&changed, value.as_bytes());
    assert_eq!(d.delimiter, Delimiter::Semicolon);
}

#[test]
fn a_remembered_header_alone_is_honoured() {
    let d = with_interpretation(b"id,name\n1,Ada\n", b"v=1;header=no");
    assert_eq!(
        (d.delimiter_source, d.header, d.header_source),
        (DialectSource::Guess, false, DialectSource::Attribute)
    );
}

#[test]
fn an_unreadable_interpretation_is_ignored_with_a_note() {
    let d = with_interpretation(b"id,name\n1,Ada\n", b"v=9;header=no");
    assert_eq!((d.header, d.header_source), (true, DialectSource::Guess));
    assert_eq!(d.notes, [Note::InterpretationUnreadable]);
}

// ---- user choices (DESIGN §3.2) -------------------------------------------

#[test]
fn user_choices_replace_the_detected_values() {
    let bytes = b"id,name\n1,Ada\n";
    let original = bytes.to_vec();
    let choices = Choices {
        delimiter: Some(Delimiter::Tab),
        header: Some(false),
        encoding: Some(Encoding::MacRoman),
    };
    let hints = Hints {
        text_encoding: Some(b"utf-8;134217984"),
        interpretation: Some(b"v=1;delimiter=pipe;header=yes"),
    };
    let d = detect(bytes, hints, choices).unwrap();
    assert_eq!(
        (d.delimiter, d.header, d.encoding),
        (Delimiter::Tab, false, Encoding::MacRoman)
    );
    assert_eq!(
        (d.delimiter_source, d.header_source, d.encoding_source),
        (
            DialectSource::User,
            DialectSource::User,
            EncodingSource::User
        )
    );
    // Detection only reads the bytes.
    assert_eq!(bytes.to_vec(), original);
}

#[test]
fn a_chosen_encoding_must_agree_with_the_bom() {
    let none = |e| {
        detect(
            b"a\n",
            Hints::default(),
            Choices {
                encoding: Some(e),
                ..Choices::default()
            },
        )
    };
    assert_eq!(
        none(Encoding::Utf16Le),
        Err(ChoiceError::EncodingDoesNotMatchBom {
            encoding: Encoding::Utf16Le,
            bom: Bom::None
        })
    );
    let bom = |e| {
        detect(
            b"\xEF\xBB\xBFa\n",
            Hints::default(),
            Choices {
                encoding: Some(e),
                ..Choices::default()
            },
        )
    };
    assert!(bom(Encoding::Utf8).is_ok());
    assert_eq!(
        bom(Encoding::Windows1252),
        Err(ChoiceError::EncodingDoesNotMatchBom {
            encoding: Encoding::Windows1252,
            bom: Bom::Utf8
        })
    );
}

/// Changing the delimiter re-reads the file's structure under it.
#[test]
fn a_chosen_delimiter_changes_what_is_read_under_it() {
    // Under ";" the quote starts a field and never closes, so the final LF
    // is inside it. Under "," the quote is in the middle of a field, so it
    // is literal and the LF ends the row.
    let bytes = b"a;\"b\n";
    let under = |d| {
        detect(
            bytes,
            Hints::default(),
            Choices {
                delimiter: Some(d),
                ..Choices::default()
            },
        )
        .unwrap()
    };
    let semi = under(Delimiter::Semicolon);
    assert_eq!(
        (semi.trailing_newline, semi.line_ending),
        (Some(false), None)
    );
    let comma = under(Delimiter::Comma);
    assert_eq!(
        (comma.trailing_newline, comma.line_ending),
        (Some(true), Some(LineEnding::Lf))
    );
}

/// A garbled key is noted, not silently skipped (`" delimiter"` would
/// otherwise look like a key from a later version).
#[test]
fn a_garbled_interpretation_key_is_noted() {
    let d = with_interpretation(b"a;b\nc;d\n", b"v=1; delimiter=comma");
    assert_eq!(
        (d.delimiter, d.delimiter_source),
        (Delimiter::Semicolon, DialectSource::Guess)
    );
    assert_eq!(d.notes, [Note::InterpretationUnreadable]);
    // A well-formed key from a later version is still ignored quietly.
    let d = with_interpretation(b"a;b\nc;d\n", b"v=1;colour=blue");
    assert_eq!(d.notes, []);
}

/// ADR-0007 decision 2: `us-ascii` reads as UTF-8.
#[test]
fn an_ascii_encoding_attribute_reads_as_utf8() {
    let d = with_encoding_attribute(b"a,b\n", b"us-ascii;1536");
    assert_eq!(
        (d.encoding, d.encoding_source, &d.notes[..]),
        (Encoding::Utf8, EncodingSource::Attribute, &[][..])
    );
}
