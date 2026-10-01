//! Detection against the testkit's generator and its statement of the
//! encoding rules (ADR-0003 decision 1, ADR-0004 decision 11).

mod common;

use common::{detect, review};
use leal_core::detect::{Choices, Detection, EncodingSource, FIRST_PAINT_BYTES, Hints};
use leal_core::dialect::Delimiter;
use leal_testkit::dialect::{self as tk, expected_encoding, reopen_encoding};
use leal_testkit::strategies::bytes::csv_bytes;
use leal_testkit::strategies::csv::{CsvConfig, GeneratedCsv, csv_file, csv_file_utf16};
use proptest::prelude::*;
use proptest::test_runner::{TestCaseError, TestRunner};

fn plain(bytes: &[u8]) -> Detection {
    detect(bytes, Hints::default(), Choices::default()).unwrap()
}

fn under_delimiter(file: &GeneratedCsv) -> Detection {
    let choices = Choices {
        delimiter: Some(common::delimiter(file.delimiter())),
        ..Choices::default()
    };
    detect(&file.bytes, Hints::default(), choices).unwrap()
}

fn any_file() -> impl Strategy<Value = GeneratedCsv> {
    prop_oneof![
        csv_file(CsvConfig::clean()),
        csv_file(CsvConfig::messy()),
        csv_file_utf16(CsvConfig::messy()),
    ]
}

/// The bytes of the other three delimiters, which would make the file's
/// own delimiter ambiguous.
fn other_delimiters(file: &GeneratedCsv) -> Vec<u8> {
    tk::Delimiter::ALL
        .into_iter()
        .filter(|d| *d != file.delimiter())
        .map(tk::Delimiter::byte)
        .collect()
}

/// A tidy file with 2 to 5 columns: unquoted words, and quoted values that
/// hold the *other* delimiters, quotes and newlines. Returns its delimiter,
/// its number of columns and its bytes.
fn tidy_file() -> impl Strategy<Value = (Delimiter, usize, Vec<u8>)> {
    let word = proptest::collection::vec(proptest::sample::select(&b"abz019 .-"[..]), 0..6);
    let tricky = proptest::collection::vec(proptest::sample::select(&b"az1 ,;|\t\"\n"[..]), 0..6)
        .prop_map(|v| {
            let mut out = vec![b'"'];
            for b in v {
                out.push(b);
                if b == b'"' {
                    out.push(b'"');
                }
            }
            out.push(b'"');
            out
        });
    let cell = prop_oneof![3 => word, 1 => tricky];
    (
        proptest::sample::select(&Delimiter::ALL[..]),
        2..6usize,
        1..12usize,
    )
        .prop_flat_map(move |(d, columns, rows)| {
            let row = proptest::collection::vec(cell.clone(), columns);
            (Just(d), Just(columns), proptest::collection::vec(row, rows))
        })
        .prop_map(|(d, columns, rows)| {
            let mut bytes = Vec::new();
            for row in rows {
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        bytes.push(d.byte());
                    }
                    // A quote in an unquoted value is mid-field; keep the
                    // file tidy by only ever writing words unquoted.
                    bytes.extend_from_slice(cell);
                }
                bytes.push(b'\n');
            }
            (d, columns, bytes)
        })
}

/// Runs `test` on values from `strategy` an eighth as many times as usual
/// (32 by default, 2,500 under `just test-deep`), for properties whose
/// cases are files of hundreds of kilobytes. The `proptest!` macro would
/// let `PROPTEST_CASES` override that, so the runner is built by hand.
fn run_large<S: Strategy>(strategy: &S, test: impl Fn(S::Value) -> Result<(), TestCaseError>) {
    let config = ProptestConfig::default(); // reads PROPTEST_CASES
    let mut runner = TestRunner::new(ProptestConfig {
        cases: (config.cases / 8).max(1),
        ..config
    });
    if let Err(e) = runner.run(strategy, test) {
        panic!("{e}");
    }
}

/// A tidy file, plus a row whose every field is a multi-line quoted value
/// full of the other delimiters, repeated past 192 KB: first paint finds
/// its delimiter, and the whole-file review, which reads quotes from the
/// start, never suggests another one (review finding 1).
#[test]
fn multi_line_fields_past_64_kb_get_no_delimiter_suggestion() {
    run_large(&tidy_file(), |(delimiter, columns, bytes)| {
        let field = b"\"one, two;\nthree|four\tfive\n\"";
        let mut extra = Vec::new();
        for i in 0..columns {
            if i > 0 {
                extra.push(delimiter.byte());
            }
            extra.extend_from_slice(field);
        }
        extra.push(b'\n');
        let copy = [bytes, extra].concat();
        let mut file = Vec::new();
        while file.len() <= FIRST_PAINT_BYTES * 3 {
            file.extend_from_slice(&copy);
        }
        let d = plain(&file);
        prop_assert_eq!(d.delimiter, delimiter);
        prop_assert_eq!(review(&file, &d).delimiter_suggestion, None);
        Ok(())
    });
}

/// Arbitrary bytes made longer than 64 KB, by padding with ASCII or by
/// repeating them: whatever first paint guessed, taking the review's
/// suggestion gives the testkit's whole-file encoding (ADR-0003 decision
/// 1, ADR-0005 decision 4).
#[test]
fn the_review_suggests_the_whole_file_encoding() {
    let files = (csv_bytes(), csv_bytes(), any::<bool>()).prop_map(|(a, b, pad)| {
        let mut file = a.clone();
        if pad {
            while file.len() <= FIRST_PAINT_BYTES {
                file.extend_from_slice(b"ascii,padding\n");
            }
            file.extend_from_slice(&b);
        } else {
            while file.len() <= FIRST_PAINT_BYTES {
                file.extend_from_slice(&b);
                file.extend_from_slice(&a);
                if a.is_empty() && b.is_empty() {
                    file.push(b'x');
                }
            }
        }
        file
    });
    run_large(&files, |file| {
        let d = plain(&file);
        let r = review(&file, &d);
        prop_assert_eq!(
            r.encoding_suggestion.unwrap_or(d.encoding),
            common::encoding(expected_encoding(&file))
        );
        Ok(())
    });
}

proptest! {
    /// In a tidy file, the guess is the file's delimiter, whatever the
    /// other delimiters inside quoted values.
    #[test]
    fn the_delimiter_guess_finds_a_tidy_file_delimiter((delimiter, _, bytes) in tidy_file()) {
        prop_assert_eq!(plain(&bytes).delimiter, delimiter);
    }
}

proptest! {
    /// Detection never panics, whatever the bytes, and the encoding follows
    /// the testkit's statement of ADR-0003 decision 1 (the files are under
    /// 64 KB, so first paint sees all of them).
    #[test]
    fn the_encoding_follows_adr_0003_on_any_bytes(bytes in csv_bytes()) {
        let d = plain(&bytes);
        prop_assert_eq!(d.encoding, common::encoding(expected_encoding(&bytes)));
        prop_assert_eq!(d.bom, common::bom(tk::Bom::detect(&bytes)));
        let r = review(&bytes, &d);
        prop_assert_eq!(r.encoding_suggestion, None);
        prop_assert_eq!(r.delimiter_suggestion, None);
        prop_assert_eq!(Some(r.trailing_newline), d.trailing_newline);
    }

    /// With a `com.apple.TextEncoding` attribute for a UTF-8, Windows-1252
    /// or UTF-16 hint, the encoding is the testkit's `reopen_encoding`
    /// (ADR-0004 decision 11): a BOM decides, a UTF-8 or Windows-1252 hint
    /// always wins, and a UTF-16 hint without a BOM is ignored.
    #[test]
    fn the_encoding_attribute_follows_adr_0004(
        bytes in csv_bytes(),
        hint in proptest::sample::select(&[
            tk::Encoding::Utf8,
            tk::Encoding::Windows1252,
            tk::Encoding::Utf16Le,
            tk::Encoding::Utf16Be,
        ][..]),
    ) {
        let value = common::text_encoding_attribute(hint);
        let hints = Hints { text_encoding: Some(&value), ..Hints::default() };
        let d = detect(&bytes, hints, Choices::default()).unwrap();
        prop_assert_eq!(d.encoding, common::encoding(reopen_encoding(&bytes, Some(hint))));
        let from_attribute = tk::Bom::detect(&bytes) == tk::Bom::None
            && matches!(hint, tk::Encoding::Utf8 | tk::Encoding::Windows1252);
        prop_assert_eq!(d.encoding_source == EncodingSource::Attribute, from_attribute);
    }

    /// Read with the file's own delimiter, detection finds the generator's
    /// BOM, encoding, line endings and trailing newline.
    #[test]
    fn the_structure_matches_the_generator(file in any_file()) {
        let d = under_delimiter(&file);
        let (line_ending, mixed) = file.layout.line_endings();
        prop_assert_eq!(d.encoding, common::encoding(file.encoding));
        prop_assert_eq!(d.bom, common::bom(tk::Bom::detect(&file.bytes)));
        prop_assert_eq!(d.line_ending, line_ending.map(common::line_ending));
        prop_assert_eq!(d.mixed_line_endings, mixed);
        prop_assert_eq!(d.trailing_newline, Some(file.layout.trailing_newline()));
        let r = review(&file.bytes, &d);
        prop_assert_eq!(r.line_ending, d.line_ending);
        prop_assert_eq!(r.mixed_line_endings, mixed);
        prop_assert_eq!(r.trailing_newline, file.layout.trailing_newline());
    }

    /// When no other delimiter byte appears anywhere and most rows have
    /// more than one field, the guess is the file's delimiter.
    #[test]
    fn the_delimiter_guess_finds_an_unambiguous_delimiter(file in any_file()) {
        let others = other_delimiters(&file);
        let unambiguous = !file.bytes.iter().any(|b| others.contains(b));
        if unambiguous && file.layout.field_count_mode().is_some_and(|m| m >= 2) {
            prop_assert_eq!(plain(&file.bytes).delimiter, common::delimiter(file.delimiter()));
        }
    }
}

/// The same file repeated past 64 KB: first paint, from the first 64 KB
/// only, finds what the whole file has, and the review agrees.
///
/// Each case is a 200 KB file, so it uses [`run_large`].
#[test]
fn a_large_file_is_detected_from_its_first_64_kb() {
    run_large(&csv_file(CsvConfig::clean()), |file| large_file_case(&file));
}

fn large_file_case(file: &GeneratedCsv) -> Result<(), TestCaseError> {
    {
        if file.layout.rows.is_empty() {
            return Ok(());
        }
        // A clean file has one kind of line ending and no unterminated
        // quote. Each copy needs a final line ending, so that copies join
        // row to row.
        let (line_ending, _) = file.layout.line_endings();
        let ending = line_ending.unwrap_or(tk::LineEnding::Lf);
        let mut body = file.bytes[file.layout.bom_len..].to_vec();
        if !file.layout.trailing_newline() {
            body.extend_from_slice(ending.bytes());
        }
        let mut large = file.bytes[..file.layout.bom_len].to_vec();
        while large.len() <= FIRST_PAINT_BYTES * 3 {
            large.extend_from_slice(&body);
        }
        let choices = Choices {
            delimiter: Some(common::delimiter(file.delimiter())),
            ..Choices::default()
        };
        let d = detect(&large, Hints::default(), choices).unwrap();
        prop_assert_eq!(d.trailing_newline, None);
        prop_assert_eq!(d.encoding, common::encoding(file.encoding));
        let source = if file.layout.bom_len > 0 {
            EncodingSource::Bom
        } else {
            EncodingSource::Guess
        };
        prop_assert_eq!(d.encoding_source, source);
        prop_assert_eq!(d.line_ending, Some(common::line_ending(ending)));
        prop_assert!(!d.mixed_line_endings);
        let r = review(&large, &d);
        prop_assert_eq!(r.encoding_suggestion, None);
        prop_assert!(r.trailing_newline);
        prop_assert_eq!(r.line_ending, d.line_ending);

        // Where the delimiter is unambiguous, the guess finds it from the
        // first 64 KB and the middle and end samples agree.
        let others = other_delimiters(file);
        if !file.bytes.iter().any(|b| others.contains(b))
            && file.layout.field_count_mode().is_some_and(|m| m >= 2)
        {
            let guessed = plain(&large);
            prop_assert_eq!(guessed.delimiter, common::delimiter(file.delimiter()));
            prop_assert_eq!(review(&large, &guessed).delimiter_suggestion, None);
        }
    }
    Ok(())
}
