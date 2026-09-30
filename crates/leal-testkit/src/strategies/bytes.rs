//! Arbitrary bytes, biased towards the bytes that matter to a CSV parser.
//!
//! Uniformly random bytes almost never contain a quote next to a delimiter,
//! a CRLF or a valid multibyte character, so they rarely reach the
//! interesting code paths. These strategies build the input from *chunks*,
//! most of which are CSV-significant.

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;

use crate::dialect::UTF8_BOM;

/// The single bytes a CSV parser treats specially: the four delimiters, the
/// quote, CR and LF.
pub const SIGNIFICANT_BYTES: [u8; 7] = *b",;\t|\"\r\n";

/// Invalid UTF-8: stray continuation bytes, bytes that never appear in
/// UTF-8, truncated sequences, an overlong encoding and an encoded
/// surrogate.
pub const INVALID_UTF8: [&[u8]; 8] = [
    b"\x80",
    b"\xBF",
    b"\xFF",
    b"\xFE",
    b"\xC3",         // lead byte of a 2-byte sequence, no continuation
    b"\xE2\x82",     // first two bytes of "€"
    b"\xC0\xAF",     // overlong "/"
    b"\xED\xA0\x80", // UTF-16 surrogate U+D800
];

/// Valid multibyte UTF-8, 2, 3 and 4 bytes long, plus U+FEFF (which is a
/// BOM only at the very start of a file).
pub const MULTIBYTE_UTF8: [&str; 6] = ["é", "ß", "€", "中", "😀", "\u{FEFF}"];

/// A strategy for arbitrary CSV-ish bytes, up to 64 chunks (a few hundred
/// bytes). See [`csv_bytes_up_to`].
pub fn csv_bytes() -> impl Strategy<Value = Vec<u8>> {
    csv_bytes_up_to(64)
}

/// A strategy for arbitrary bytes built from up to `max_chunks` chunks. Each
/// chunk is one of:
///
/// - a CSV-significant byte ([`SIGNIFICANT_BYTES`]) or `\r\n`;
/// - a short run of ASCII letters, digits and spaces;
/// - any byte at all;
/// - NUL;
/// - a UTF-8 BOM (anywhere, not only at the start);
/// - invalid UTF-8 ([`INVALID_UTF8`]);
/// - valid multibyte UTF-8 ([`MULTIBYTE_UTF8`]).
///
/// One input in five also starts with a UTF-8 BOM. Adjacent chunks can
/// combine (a truncated sequence followed by a continuation byte may become
/// valid), which is fine: the aim is coverage, not a precise mix.
pub fn csv_bytes_up_to(max_chunks: usize) -> impl Strategy<Value = Vec<u8>> {
    (prop::bool::weighted(0.2), vec(chunk(), 0..=max_chunks)).prop_map(|(bom, chunks)| {
        let mut out = Vec::new();
        if bom {
            out.extend_from_slice(UTF8_BOM);
        }
        for c in chunks {
            out.extend_from_slice(&c);
        }
        out
    })
}

fn chunk() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        8 => select(&SIGNIFICANT_BYTES[..]).prop_map(|b| vec![b]),
        2 => Just(b"\r\n".to_vec()),
        6 => vec(select(&b"abcxyzABC019 "[..]), 1..=4),
        3 => any::<u8>().prop_map(|b| vec![b]),
        1 => Just(vec![0u8]),
        1 => Just(UTF8_BOM.to_vec()),
        2 => select(&INVALID_UTF8[..]).prop_map(<[u8]>::to_vec),
        2 => select(&MULTIBYTE_UTF8[..]).prop_map(|s| s.as_bytes().to_vec()),
    ]
}
