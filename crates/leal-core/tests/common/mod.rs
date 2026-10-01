//! Conversions between the testkit's vocabulary and leal-core's, shared by
//! the detection tests. The testkit doesn't depend on leal-core (it is an
//! independent oracle), so the two have their own copies of these types.

#![allow(dead_code)] // each test file uses a different subset

use std::sync::atomic::AtomicBool;

use leal_core::detect::{ChoiceError, Choices, Detection, Hints, Review};
use leal_core::dialect::{Bom, Delimiter, Encoding, LineEnding};
use leal_testkit::dialect as tk;

/// `leal_core::detect::detect` on a whole file in memory.
pub fn detect(file: &[u8], hints: Hints<'_>, choices: Choices) -> Result<Detection, ChoiceError> {
    leal_core::detect::detect(file, len(file), hints, choices)
}

/// `leal_core::detect::review`, never cancelled.
pub fn review(file: &[u8], detection: &Detection) -> Review {
    leal_core::detect::review(file, detection, &AtomicBool::new(false)).expect("nothing cancels it")
}

/// A slice's length as a file length.
pub fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).expect("fits")
}

pub fn delimiter(d: tk::Delimiter) -> Delimiter {
    Delimiter::from_byte(d.byte()).expect("the testkit's delimiters are Leal's")
}

pub fn encoding(e: tk::Encoding) -> Encoding {
    match e {
        tk::Encoding::Utf8 => Encoding::Utf8,
        tk::Encoding::Utf16Le => Encoding::Utf16Le,
        tk::Encoding::Utf16Be => Encoding::Utf16Be,
        tk::Encoding::Windows1252 => Encoding::Windows1252,
    }
}

pub fn bom(b: tk::Bom) -> Bom {
    match b {
        tk::Bom::None => Bom::None,
        tk::Bom::Utf8 => Bom::Utf8,
        tk::Bom::Utf16Le => Bom::Utf16Le,
        tk::Bom::Utf16Be => Bom::Utf16Be,
    }
}

pub fn line_ending(l: tk::LineEnding) -> LineEnding {
    match l {
        tk::LineEnding::Lf => LineEnding::Lf,
        tk::LineEnding::Crlf => LineEnding::Crlf,
        tk::LineEnding::Cr => LineEnding::Cr,
    }
}

/// The `com.apple.TextEncoding` value TextEdit writes for the testkit's
/// encoding hint.
pub fn text_encoding_attribute(e: tk::Encoding) -> Vec<u8> {
    leal_core::attributes::text_encoding_value(encoding(e)).into_bytes()
}
