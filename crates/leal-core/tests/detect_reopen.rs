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

use leal_core::attributes::{Fingerprint, Interpretation};
use leal_core::detect::{Choices, Detection, DialectSource, Hints, detect};
use leal_testkit::strategies::csv::CsvConfig;
use leal_testkit::strategies::edits::{EditCase, edit_case};
use proptest::prelude::*;

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
        file: Some(Fingerprint::of(saved)),
    })
}

proptest! {
    #[test]
    fn reopening_a_saved_file_gives_the_same_interpretation(
        case in prop_oneof![edit_case(CsvConfig::clean()), edit_case(CsvConfig::messy())]
    ) {
        let Ok(saved) = &case.saved else {
            return Ok(()); // F5 failures and read-only files have no output
        };
        let document = opened(&case);
        let text_encoding = saved.encoding_hint.map(common::text_encoding_attribute);
        let interpretation = interpretation_to_write(&saved.bytes, text_encoding.as_deref(), &document)
            .map(|i| i.to_attribute_value().into_bytes());
        let hints = Hints {
            text_encoding: text_encoding.as_deref(),
            interpretation: interpretation.as_deref(),
        };
        let reopened = detect(&saved.bytes, hints, Choices::default()).unwrap();

        prop_assert_eq!(reopened.delimiter, document.delimiter);
        prop_assert_eq!(reopened.header, document.header);
        prop_assert_eq!(reopened.encoding, common::encoding(case.file.encoding));
        prop_assert_eq!(reopened.bom, document.bom);
        prop_assert_eq!(&reopened.notes, &[]);
    }
}
