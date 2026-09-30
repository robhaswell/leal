//! Dialect vocabulary shared by the generator, the corpus sidecars and tests.
//!
//! These are deliberately small, test-side copies of concepts that `leal-core`
//! will define for itself in task 1.2. The testkit does not depend on
//! `leal-core`, so that it can act as an independent oracle.

use serde::Deserialize;

/// The UTF-8 byte order mark, `EF BB BF`.
pub const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// The UTF-16 little-endian byte order mark, `FF FE`.
pub const UTF16LE_BOM: &[u8] = b"\xFF\xFE";
/// The UTF-16 big-endian byte order mark, `FE FF`.
pub const UTF16BE_BOM: &[u8] = b"\xFE\xFF";

/// A field delimiter Leal detects (DESIGN §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub enum Delimiter {
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `\t`
    Tab,
    /// `|`
    Pipe,
}

impl Delimiter {
    /// Every delimiter, in detection-preference order.
    pub const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Semicolon,
        Delimiter::Tab,
        Delimiter::Pipe,
    ];

    /// The delimiter's byte.
    #[must_use]
    pub fn byte(self) -> u8 {
        match self {
            Delimiter::Comma => b',',
            Delimiter::Semicolon => b';',
            Delimiter::Tab => b'\t',
            Delimiter::Pipe => b'|',
        }
    }

    /// The delimiter for `byte`, if it is one of the four Leal detects.
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.byte() == byte)
    }
}

impl TryFrom<String> for Delimiter {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        match s.as_bytes() {
            [b] => Delimiter::from_byte(*b),
            _ => None,
        }
        .ok_or_else(|| {
            format!("unknown delimiter {s:?}; expected one of \",\" \";\" \"\\t\" \"|\"")
        })
    }
}

/// A line ending. A CR immediately followed by LF is always one CRLF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    Crlf,
    /// `\r` on its own
    Cr,
}

impl LineEnding {
    /// Every line ending.
    pub const ALL: [LineEnding; 3] = [LineEnding::Lf, LineEnding::Crlf, LineEnding::Cr];

    /// The line ending's bytes.
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        match self {
            LineEnding::Lf => b"\n",
            LineEnding::Crlf => b"\r\n",
            LineEnding::Cr => b"\r",
        }
    }

    /// The number of bytes in the line ending (1 or 2).
    #[must_use]
    pub fn byte_len(self) -> usize {
        self.bytes().len()
    }
}

/// A byte order mark at the very start of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
pub enum Bom {
    /// No BOM.
    #[serde(rename = "none")]
    None,
    /// `EF BB BF`
    #[serde(rename = "utf-8")]
    Utf8,
    /// `FF FE`
    #[serde(rename = "utf-16le")]
    Utf16Le,
    /// `FE FF`
    #[serde(rename = "utf-16be")]
    Utf16Be,
}

impl Bom {
    /// The BOM's bytes (empty for [`Bom::None`]).
    #[must_use]
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Bom::None => b"",
            Bom::Utf8 => UTF8_BOM,
            Bom::Utf16Le => UTF16LE_BOM,
            Bom::Utf16Be => UTF16BE_BOM,
        }
    }

    /// The BOM at the start of `bytes`. UTF-8 is checked first; `FF FE` is
    /// always taken as UTF-16 LE (UTF-32 is out of scope).
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Self {
        [Bom::Utf8, Bom::Utf16Le, Bom::Utf16Be]
            .into_iter()
            .find(|bom| bytes.starts_with(bom.bytes()))
            .unwrap_or(Bom::None)
    }
}

/// A text encoding Leal reads (DESIGN §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
pub enum Encoding {
    /// UTF-8, with or without a BOM.
    #[serde(rename = "utf-8")]
    Utf8,
    /// UTF-16 little-endian (read-only in v1).
    #[serde(rename = "utf-16le")]
    Utf16Le,
    /// UTF-16 big-endian (read-only in v1).
    #[serde(rename = "utf-16be")]
    Utf16Be,
    /// Windows-1252, the default single-byte fallback.
    #[serde(rename = "windows-1252")]
    Windows1252,
}

impl Encoding {
    /// True for the encodings in which every structural byte (`,` `;` `\t`
    /// `|` `"` CR LF) is a single ASCII byte, so a parser can work on the
    /// raw bytes. False for UTF-16.
    #[must_use]
    pub fn is_ascii_compatible(self) -> bool {
        matches!(self, Encoding::Utf8 | Encoding::Windows1252)
    }
}

/// Counts of valid multibyte UTF-8 sequences and of invalid bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Utf8Census {
    /// Valid sequences of 2 to 4 bytes (U+FEFF included).
    pub multibyte: usize,
    /// Bytes that are part of no valid sequence.
    pub invalid_bytes: usize,
}

impl Utf8Census {
    /// Counts `bytes`. Invalid sequences are split as `from_utf8_lossy`
    /// splits them; every byte of each one counts.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        let mut census = Utf8Census::default();
        let mut rest = bytes;
        loop {
            let (valid, next) = match std::str::from_utf8(rest) {
                Ok(s) => (s, None),
                Err(e) => {
                    let valid_len = e.valid_up_to();
                    let bad = e.error_len().unwrap_or(rest.len() - valid_len);
                    census.invalid_bytes += bad;
                    // The prefix was just checked, so this cannot fail.
                    let valid = std::str::from_utf8(&rest[..valid_len]).unwrap_or_default();
                    (valid, Some(valid_len + bad))
                }
            };
            census.multibyte += valid.chars().filter(|c| c.len_utf8() > 1).count();
            match next {
                Some(n) => rest = &rest[n..],
                None => return census,
            }
        }
    }
}

/// The encoding ADR-0003 (decision 1) says a file has. This is the
/// testkit's statement of the rule, not the product's detector (task 1.2).
///
/// - A BOM decides: UTF-8, UTF-16 LE or UTF-16 BE.
/// - Pure ASCII is UTF-8.
/// - A file with at least one valid multibyte UTF-8 sequence, and more of
///   them than invalid bytes, is UTF-8 (with an invalid-encoding warning if
///   any bytes are invalid).
/// - Anything else is Windows-1252, the single-byte default.
#[must_use]
pub fn expected_encoding(bytes: &[u8]) -> Encoding {
    match Bom::detect(bytes) {
        Bom::Utf8 => return Encoding::Utf8,
        Bom::Utf16Le => return Encoding::Utf16Le,
        Bom::Utf16Be => return Encoding::Utf16Be,
        Bom::None => {}
    }
    let census = Utf8Census::of(bytes);
    let ascii = census.multibyte == 0 && census.invalid_bytes == 0;
    if ascii || (census.multibyte >= 1 && census.multibyte > census.invalid_bytes) {
        Encoding::Utf8
    } else {
        Encoding::Windows1252
    }
}

/// Windows-1252 bytes 0x80–0x9F, as the WHATWG Encoding Standard maps them.
/// The five unassigned bytes map to the C1 control of the same value; every
/// other byte maps to the code point of the same value.
const WINDOWS_1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// Decodes Windows-1252 (WHATWG). Every byte decodes.
#[must_use]
pub fn decode_windows_1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => WINDOWS_1252_HIGH[usize::from(b - 0x80)],
            _ => char::from(b),
        })
        .collect()
}

/// Decodes bytes in an ASCII-compatible encoding for display: UTF-8 with
/// invalid sequences as U+FFFD, or Windows-1252. For UTF-16 the bytes are
/// taken as UTF-8, because the testkit stores UTF-16 values transcoded (see
/// [`crate::layout::FieldLayout::value`]).
#[must_use]
pub fn decode_value(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Windows1252 => decode_windows_1252(bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Encodes `text` in `encoding` for writing into a file (DESIGN §3.7).
///
/// # Errors
///
/// Returns the first character Windows-1252 cannot represent. UTF-16 files
/// are read-only in v1, so encoding into UTF-16 is always an error (with
/// the first character, or U+0000 for empty text).
pub fn encode_value(text: &str, encoding: Encoding) -> Result<Vec<u8>, char> {
    match encoding {
        Encoding::Utf8 => Ok(text.as_bytes().to_vec()),
        Encoding::Windows1252 => text
            .chars()
            .map(|c| {
                if let Some(i) = WINDOWS_1252_HIGH.iter().position(|&h| h == c) {
                    // i < 32, so this cannot truncate.
                    return u8::try_from(0x80 + i).map_err(|_| c);
                }
                match u32::from(c) {
                    0x00..=0x7F | 0xA0..=0xFF => u8::try_from(u32::from(c)).map_err(|_| c),
                    _ => Err(c),
                }
            })
            .collect(),
        Encoding::Utf16Le | Encoding::Utf16Be => Err(text.chars().next().unwrap_or('\0')),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_1252_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255).collect();
        let text = decode_windows_1252(&all);
        assert_eq!(text.chars().count(), 256);
        assert_eq!(encode_value(&text, Encoding::Windows1252), Ok(all));
        assert_eq!(
            encode_value("€é", Encoding::Windows1252),
            Ok(vec![0x80, 0xE9])
        );
        assert_eq!(encode_value("a😀", Encoding::Windows1252), Err('😀'));
        assert_eq!(encode_value("Ā", Encoding::Windows1252), Err('Ā'));
        assert_eq!(encode_value("x", Encoding::Utf16Le), Err('x'));
        assert_eq!(
            encode_value("é", Encoding::Utf8),
            Ok("é".as_bytes().to_vec())
        );
    }

    #[test]
    fn utf8_census_counts_sequences_and_bytes() {
        assert_eq!(Utf8Census::of(b""), Utf8Census::default());
        assert_eq!(
            Utf8Census::of("aé€😀".as_bytes()),
            Utf8Census {
                multibyte: 3,
                invalid_bytes: 0
            }
        );
        // A truncated "€" (2 bytes), then "é", then a stray 0xFF.
        assert_eq!(
            Utf8Census::of(b"\xE2\x82x\xC3\xA9\xFF"),
            Utf8Census {
                multibyte: 1,
                invalid_bytes: 3
            }
        );
    }

    #[test]
    fn expected_encoding_follows_adr_0003() {
        assert_eq!(expected_encoding(b""), Encoding::Utf8);
        assert_eq!(expected_encoding(b"a,b\n"), Encoding::Utf8);
        assert_eq!(expected_encoding("é,ü\n".as_bytes()), Encoding::Utf8);
        // Two valid multibyte sequences outnumber one invalid byte.
        assert_eq!(expected_encoding(b"\xC3\xA9\xC3\xBC\xE9"), Encoding::Utf8);
        // A tie is not "outnumber".
        assert_eq!(expected_encoding(b"\xC3\xA9\xE9"), Encoding::Windows1252);
        // High bytes with no valid multibyte sequence at all.
        assert_eq!(expected_encoding(b"Caf\xE9"), Encoding::Windows1252);
        assert_eq!(expected_encoding(b"\xEF\xBB\xBF\xFF"), Encoding::Utf8);
        assert_eq!(expected_encoding(b"\xFF\xFEa\0"), Encoding::Utf16Le);
        assert_eq!(expected_encoding(b"\xFE\xFF\0a"), Encoding::Utf16Be);
    }

    #[test]
    fn delimiter_bytes_round_trip() {
        for d in Delimiter::ALL {
            assert_eq!(Delimiter::from_byte(d.byte()), Some(d));
        }
        assert_eq!(Delimiter::from_byte(b':'), None);
    }

    #[test]
    fn delimiter_from_string() {
        assert_eq!(Delimiter::try_from("\t".to_owned()), Ok(Delimiter::Tab));
        assert!(Delimiter::try_from(",,".to_owned()).is_err());
        assert!(Delimiter::try_from(String::new()).is_err());
    }

    #[test]
    fn bom_detection() {
        assert_eq!(Bom::detect(b"\xEF\xBB\xBFa"), Bom::Utf8);
        assert_eq!(Bom::detect(b"\xFF\xFEa\0"), Bom::Utf16Le);
        assert_eq!(Bom::detect(b"\xFE\xFF\0a"), Bom::Utf16Be);
        assert_eq!(Bom::detect(b"\xEF\xBB"), Bom::None);
        assert_eq!(Bom::detect(b""), Bom::None);
    }
}
