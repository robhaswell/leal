//! The reopen property (ADR-0004 decisions 10 and 11, ADR-0005 decision 1)
//! with the real detector: after any generated edit, reopening the saved
//! file gives the document's delimiter, header choice and encoding.
//!
//! The testkit's own reopen property (`leal-testkit/tests/edits.rs`) can't
//! check this, because the testkit never calls product code. This test
//! replays its edit cases and save oracle, and models the two attributes
//! the way the testkit models the encoding hint:
//!
//! - `com.apple.TextEncoding` is the oracle's `SavedFile::encoding_hint`,
//!   written in TextEdit's format;
//! - Leal's interpretation attribute is written when a reopen without it
//!   would guess a different delimiter or header, or when the document's
//!   choice came from the user or an earlier attribute (ADR-0005
//!   decision 1), with the saved file's fingerprint. Writing it for real is
//!   task 2.5.

mod common;

use common::detect;
use leal_core::attributes::{Fingerprint, Interpretation};
use leal_core::detect::{Choices, Detection, DialectSource, Hints, Note};
use leal_core::dialect::Delimiter;
use leal_testkit::strategies::csv::CsvConfig;
use leal_testkit::strategies::edits::{EditCase, edit_case};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{RngAlgorithm, TestCaseError, TestRng, TestRunner};

/// The interpretation the document had when it was opened. The save
/// oracle writes with the file's own delimiter, so the model opens it with
/// that delimiter: guessed if detection finds it, otherwise chosen by the
/// user. The header is whatever detection decides under that delimiter.
fn opened(case: &EditCase) -> Detection {
    let delimiter = common::delimiter(case.file.delimiter());
    let guessed = detect(&case.file.bytes, Hints::default(), Choices::default()).unwrap();
    if guessed.delimiter == delimiter {
        return guessed;
    }
    let choices = Choices {
        delimiter: Some(delimiter),
        ..Choices::default()
    };
    detect(&case.file.bytes, Hints::default(), choices).unwrap()
}

/// The interpretation attribute a save writes (ADR-0005 decision 1). A
/// reopen reads the saved `com.apple.TextEncoding` too, which can change
/// the header guess (the cells decode differently), so it is passed in.
fn interpretation_to_write(
    saved: &[u8],
    text_encoding: Option<&[u8]>,
    document: &Detection,
    fingerprint: bool,
) -> Option<Interpretation> {
    let hints = Hints {
        text_encoding,
        ..Hints::default()
    };
    let reopened = detect(saved, hints, Choices::default()).unwrap();
    let chosen = document.delimiter_source != DialectSource::Guess
        || document.header_source != DialectSource::Guess;
    let differs = reopened.delimiter != document.delimiter || reopened.header != document.header;
    (chosen || differs).then_some(Interpretation {
        delimiter: Some(document.delimiter),
        header: Some(document.header),
        file: fingerprint.then(|| Fingerprint::of(saved)),
        encoding: None,
    })
}

/// How a reopen with the remembered interpretation went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// No attribute was needed.
    NotWritten,
    /// The attribute was used.
    Honoured,
    /// The attribute was ignored as no longer fitting.
    Rejected,
}

/// Saves `case`, writes the attributes, reopens, and checks the result.
/// With `fingerprint` the reopen must give the document's interpretation.
/// Without it (as if another program had touched the file since, keeping
/// the attribute), the attribute goes through the "parses sensibly" check
/// (ADR-0005 decision 1), and is either used as is or ignored with a note
/// in favour of the plain guess; nothing in between.
fn reopen(case: &EditCase, fingerprint: bool) -> Result<Outcome, TestCaseError> {
    let Ok(saved) = &case.saved else {
        return Ok(Outcome::NotWritten); // F5 failures and read-only files
    };
    let document = opened(case);
    let text_encoding = saved.encoding_hint.map(common::text_encoding_attribute);
    let interpretation = interpretation_to_write(
        &saved.bytes,
        text_encoding.as_deref(),
        &document,
        fingerprint,
    )
    .map(|i| i.to_attribute_value().into_bytes());
    let hints = Hints {
        text_encoding: text_encoding.as_deref(),
        interpretation: interpretation.as_deref(),
    };
    let reopened = detect(&saved.bytes, hints, Choices::default()).unwrap();
    prop_assert_eq!(reopened.encoding, common::encoding(case.file.encoding));
    prop_assert_eq!(reopened.bom, document.bom);

    let honoured = reopened.notes.is_empty();
    if fingerprint || honoured {
        prop_assert_eq!(reopened.delimiter, document.delimiter);
        prop_assert_eq!(reopened.header, document.header);
        prop_assert_eq!(&reopened.notes, &[]);
        return Ok(if interpretation.is_some() {
            Outcome::Honoured
        } else {
            Outcome::NotWritten
        });
    }
    let without = Hints {
        text_encoding: text_encoding.as_deref(),
        ..Hints::default()
    };
    let guess = detect(&saved.bytes, without, Choices::default()).unwrap();
    prop_assert_eq!(
        &reopened.notes,
        &[Note::InterpretationNotSensible {
            delimiter: document.delimiter
        }]
    );
    prop_assert_eq!(reopened.delimiter, guess.delimiter);
    prop_assert_eq!(reopened.header, guess.header);
    prop_assert_eq!(reopened.delimiter_source, DialectSource::Guess);
    Ok(Outcome::Rejected)
}

fn cases() -> impl Strategy<Value = EditCase> {
    prop_oneof![edit_case(CsvConfig::clean()), edit_case(CsvConfig::messy())]
}

proptest! {
    #[test]
    fn reopening_a_saved_file_gives_the_same_interpretation(case in cases()) {
        reopen(&case, true)?;
    }

    /// The same, with no fingerprint in the attribute: the "parses
    /// sensibly" path that an outside edit leads to.
    #[test]
    fn reopening_after_an_outside_edit_uses_or_ignores_the_attribute(case in cases()) {
        reopen(&case, false)?;
    }
}

/// How often the "parses sensibly" check rejects an interpretation Leal
/// remembered, when the fingerprint can't vouch for the file: rarely, and
/// only in the tiny, ambiguous files the generator makes. Fixed seed.
#[test]
fn the_sensible_check_rarely_rejects_a_remembered_interpretation() {
    let mut runner = TestRunner::new_with_rng(
        ProptestConfig::with_cases(2000),
        TestRng::deterministic_rng(RngAlgorithm::ChaCha),
    );
    let strategy = cases();
    let (mut honoured, mut rejected) = (0, 0);
    for _ in 0..2000 {
        let case = strategy.new_tree(&mut runner).unwrap().current();
        match reopen(&case, false).unwrap() {
            Outcome::Honoured => honoured += 1,
            Outcome::Rejected => rejected += 1,
            Outcome::NotWritten => {}
        }
    }
    eprintln!("remembered interpretations: {honoured} honoured, {rejected} rejected");
    assert!(honoured >= 100, "{honoured} honoured");
    assert!(
        rejected * 10 <= honoured,
        "{rejected} rejected of {}",
        honoured + rejected
    );
}

/// How `delimiter` splits a small file's non-blank rows, by the test's own
/// count: (the most common field count, ties to the first seen; the rows
/// with it; all non-blank rows), or `None` with no non-blank rows.
///
/// It follows DESIGN §3.4 independently of the product's scanner: a quote
/// opens a quoted field only as its first byte, `""` inside is a quote,
/// anything after the closing quote is literal, CR LF is one line ending,
/// and an open quote runs to the end.
fn split(bytes: &[u8], delimiter: u8) -> Option<(usize, usize, usize)> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut counts: Vec<usize> = Vec::new(); // each non-blank row's fields
    let mut i = 0;
    while i < bytes.len() {
        let row_start = i;
        let mut fields = 1;
        loop {
            if bytes.get(i) == Some(&b'"') {
                i += 1;
                while i < bytes.len() {
                    i += 1;
                    if bytes[i - 1] == b'"' {
                        if bytes.get(i) != Some(&b'"') {
                            break;
                        }
                        i += 1;
                    }
                }
            }
            while i < bytes.len() && ![delimiter, b'\r', b'\n'].contains(&bytes[i]) {
                i += 1;
            }
            if bytes.get(i) != Some(&delimiter) {
                break;
            }
            fields += 1;
            i += 1;
        }
        let blank = i == row_start;
        if bytes.get(i) == Some(&b'\r') && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
        }
        i += 1;
        if !blank {
            counts.push(fields);
        }
    }
    let rows_with = |c: usize| counts.iter().filter(|d| **d == c).count();
    // `max_by_key` keeps the last of equals; reversed, the first seen.
    let mode = counts.iter().rev().copied().max_by_key(|c| rows_with(*c))?;
    Some((mode, rows_with(mode), counts.len()))
}

proptest! {
    /// ADR-0005 decision 1's "parses sensibly" check itself, on saved
    /// files: for every delimiter other than the guess, an attribute
    /// remembering it (without a fingerprint, so the check applies) is used
    /// exactly when it splits the rows at least as consistently as the
    /// guess, by the test's own count, or when the guess splits nothing
    /// (a one-column file, whose `,` is only the default). Every case
    /// reaches the comparison three times (re-review finding 3).
    #[test]
    fn a_remembered_delimiter_is_used_exactly_when_it_fits(case in cases()) {
        let Ok(saved) = &case.saved else {
            return Ok(());
        };
        let bytes = &saved.bytes;
        let guess = detect(bytes, Hints::default(), Choices::default())
            .unwrap()
            .delimiter;
        for remembered in Delimiter::ALL.into_iter().filter(|d| *d != guess) {
            let value = Interpretation {
                delimiter: Some(remembered),
                header: None,
                file: None,
                encoding: None,
            }
            .to_attribute_value();
            let hints = Hints {
                interpretation: Some(value.as_bytes()),
                ..Hints::default()
            };
            let reopened = detect(bytes, hints, Choices::default()).unwrap();
            let fits = match (split(bytes, remembered.byte()), split(bytes, guess.byte())) {
                (Some((_, r, r_rows)), Some((g_mode, g, g_rows))) => {
                    g_mode < 2 || r * g_rows >= g * r_rows
                }
                _ => true,
            };
            if fits {
                prop_assert_eq!(reopened.delimiter, remembered);
                prop_assert_eq!(reopened.delimiter_source, DialectSource::Attribute);
                prop_assert_eq!(&reopened.notes, &[]);
            } else {
                prop_assert_eq!(reopened.delimiter, guess);
                prop_assert_eq!(
                    &reopened.notes,
                    &[Note::InterpretationNotSensible { delimiter: remembered }]
                );
            }
        }
    }
}
