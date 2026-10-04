//! Detection and the whole-file review on arbitrary bytes: no panic, the
//! same answer every time, and the testkit's encoding rules: ADR-0003
//! decision 1 with no attribute, ADR-0004 decision 11 with one. A chosen
//! encoding is accepted exactly when the BOM allows it, and a chosen
//! delimiter or encoding is the one used. The bytes also serve as
//! (garbled) attribute values, and as the head of a longer file.

#![no_main]

use std::sync::atomic::AtomicBool;

use leal_core::attributes::text_encoding_value;
use leal_core::detect::{Choices, Detection, FIRST_PAINT_BYTES, Hints, Review, detect, review};
use leal_core::dialect::{Bom, Delimiter, Encoding};
use leal_fuzz::{core_encoding, tk_encoding};
use leal_testkit::dialect::{self as tk, HintWriter, expected_encoding, reopen_encoding};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let len = u64::try_from(bytes.len()).expect("fits");
    let whole = bytes.len() <= FIRST_PAINT_BYTES;

    // No attributes and no choices: ADR-0003 decision 1.
    let plain =
        twice(bytes, len, Hints::default(), Choices::default()).expect("no choices to refuse");
    let bom = Bom::detect(bytes);
    assert_eq!(plain.bom, bom);
    if whole {
        assert_eq!(
            tk_encoding(plain.encoding),
            expected_encoding(bytes),
            "ADR-0003 decision 1"
        );
        let r = review_twice(bytes, &plain);
        assert_eq!(r.encoding_suggestion, None, "the head was the whole file");
        assert_eq!(r.delimiter_suggestion, None, "the head was the whole file");
        assert_eq!(Some(r.trailing_newline), plain.trailing_newline);
    }

    // The attribute TextEdit writes, for the encodings ADR-0004 decision
    // 11 covers: the testkit's `reopen_encoding`.
    for hint in [
        tk::Encoding::Utf8,
        tk::Encoding::Windows1252,
        tk::Encoding::Utf16Le,
        tk::Encoding::Utf16Be,
    ] {
        let value = text_encoding_value(core_encoding(hint)).into_bytes();
        let hints = Hints {
            text_encoding: Some(&value),
            interpretation: None,
        };
        let d = twice(bytes, len, hints, Choices::default()).expect("no choices to refuse");
        if whole {
            let expected = reopen_encoding(bytes, Some(hint), HintWriter::OtherApp);
            assert_eq!(
                tk_encoding(d.encoding),
                expected,
                "ADR-0004 decision 11, hint {hint:?}"
            );
        }
    }

    // Every choice: a chosen encoding is refused exactly when the BOM
    // doesn't allow it; otherwise it, and the chosen delimiter, are used.
    for (i, encoding) in Encoding::ALL.into_iter().enumerate() {
        let delimiter = Delimiter::ALL[i % Delimiter::ALL.len()];
        let choices = Choices {
            delimiter: Some(delimiter),
            header: Some(i % 2 == 0),
            encoding: Some(encoding),
        };
        match twice(bytes, len, Hints::default(), choices) {
            Ok(d) => {
                assert!(
                    bom.allows(encoding),
                    "{encoding:?} accepted with BOM {bom:?}"
                );
                assert_eq!(
                    (d.encoding, d.delimiter, d.header),
                    (encoding, delimiter, i % 2 == 0)
                );
                review_twice(bytes, &d);
            }
            Err(_) => assert!(
                !bom.allows(encoding),
                "{encoding:?} refused with BOM {bom:?}"
            ),
        }
    }

    // The bytes themselves as both attributes (garbled, usually), and as
    // the first part of a longer file.
    let split = bytes.len() / 2;
    let hints = Hints {
        text_encoding: Some(&bytes[..split]),
        interpretation: Some(&bytes[split..]),
    };
    let d = twice(bytes, len, hints, Choices::default()).expect("no choices to refuse");
    review_twice(bytes, &d);
    twice(
        bytes,
        len.saturating_mul(2).saturating_add(1),
        hints,
        Choices::default(),
    )
    .expect("no choices to refuse");
});

/// Detection, run twice: the same answer both times.
fn twice(
    bytes: &[u8],
    len: u64,
    hints: Hints<'_>,
    choices: Choices,
) -> Result<Detection, leal_core::detect::ChoiceError> {
    let first = detect(bytes, len, hints, choices);
    assert_eq!(
        first,
        detect(bytes, len, hints, choices),
        "detection is deterministic"
    );
    first
}

/// The review, run twice: the same answer both times.
fn review_twice(bytes: &[u8], detection: &Detection) -> Review {
    let first = review(bytes, detection, &AtomicBool::new(false)).expect("never cancelled");
    let second = review(bytes, detection, &AtomicBool::new(false)).expect("never cancelled");
    assert_eq!(first, second, "the review is deterministic");
    first
}
