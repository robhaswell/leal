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
