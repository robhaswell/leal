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

/// A text encoding Leal reads (DESIGN §3.2, ADR-0005 decision 5).
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
    /// Windows-1250 (like the rest below, only from the attribute or the
    /// user).
    #[serde(rename = "windows-1250")]
    Windows1250,
    /// Windows-1251.
    #[serde(rename = "windows-1251")]
    Windows1251,
    /// Windows-1253.
    #[serde(rename = "windows-1253")]
    Windows1253,
    /// Windows-1254.
    #[serde(rename = "windows-1254")]
    Windows1254,
    /// Windows-1255.
    #[serde(rename = "windows-1255")]
    Windows1255,
    /// Windows-1256.
    #[serde(rename = "windows-1256")]
    Windows1256,
    /// Windows-1257.
    #[serde(rename = "windows-1257")]
    Windows1257,
    /// Windows-1258.
    #[serde(rename = "windows-1258")]
    Windows1258,
    /// ISO-8859-1: every byte is the code point of the same value.
    #[serde(rename = "iso-8859-1")]
    Iso8859_1,
    /// ISO-8859-2.
    #[serde(rename = "iso-8859-2")]
    Iso8859_2,
    /// ISO-8859-15.
    #[serde(rename = "iso-8859-15")]
    Iso8859_15,
    /// Mac Roman.
    #[serde(rename = "macintosh")]
    MacRoman,
}

impl Encoding {
    /// The single-byte encodings Leal reads (ADR-0005 decision 5): every
    /// encoding but UTF-8 and UTF-16.
    pub const SINGLE_BYTE: [Encoding; 13] = [
        Encoding::Windows1252,
        Encoding::Windows1250,
        Encoding::Windows1251,
        Encoding::Windows1253,
        Encoding::Windows1254,
        Encoding::Windows1255,
        Encoding::Windows1256,
        Encoding::Windows1257,
        Encoding::Windows1258,
        Encoding::Iso8859_1,
        Encoding::Iso8859_2,
        Encoding::Iso8859_15,
        Encoding::MacRoman,
    ];

    /// True for the encodings in which every structural byte (`,` `;` `\t`
    /// `|` `"` CR LF) is a single ASCII byte, so a parser can work on the
    /// raw bytes. False for UTF-16.
    #[must_use]
    pub fn is_ascii_compatible(self) -> bool {
        !matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }

    /// True for the single-byte encodings ([`Encoding::SINGLE_BYTE`]).
    #[must_use]
    pub fn is_single_byte(self) -> bool {
        !matches!(self, Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be)
    }

    /// The encoding's IANA name, as macOS writes it in
    /// `com.apple.TextEncoding` (and as the corpus sidecars spell it).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Windows1252 => "windows-1252",
            Encoding::Windows1250 => "windows-1250",
            Encoding::Windows1251 => "windows-1251",
            Encoding::Windows1253 => "windows-1253",
            Encoding::Windows1254 => "windows-1254",
            Encoding::Windows1255 => "windows-1255",
            Encoding::Windows1256 => "windows-1256",
            Encoding::Windows1257 => "windows-1257",
            Encoding::Windows1258 => "windows-1258",
            Encoding::Iso8859_1 => "iso-8859-1",
            Encoding::Iso8859_2 => "iso-8859-2",
            Encoding::Iso8859_15 => "iso-8859-15",
            Encoding::MacRoman => "macintosh",
        }
    }

    /// The WHATWG table of a single-byte encoding other than Windows-1252
    /// (typed out in this module) and ISO-8859-1 (every byte its own code
    /// point).
    fn whatwg(self) -> Option<&'static encoding_rs::Encoding> {
        Some(match self {
            Encoding::Windows1250 => encoding_rs::WINDOWS_1250,
            Encoding::Windows1251 => encoding_rs::WINDOWS_1251,
            Encoding::Windows1253 => encoding_rs::WINDOWS_1253,
            Encoding::Windows1254 => encoding_rs::WINDOWS_1254,
            Encoding::Windows1255 => encoding_rs::WINDOWS_1255,
            Encoding::Windows1256 => encoding_rs::WINDOWS_1256,
            Encoding::Windows1257 => encoding_rs::WINDOWS_1257,
            Encoding::Windows1258 => encoding_rs::WINDOWS_1258,
            Encoding::Iso8859_2 => encoding_rs::ISO_8859_2,
            Encoding::Iso8859_15 => encoding_rs::ISO_8859_15,
            Encoding::MacRoman => encoding_rs::MACINTOSH,
            _ => return None,
        })
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

/// The encoding a reopen uses, given the file's bytes and its encoding hint
/// (ADR-0004 decision 11). The hint models the macOS
/// `com.apple.TextEncoding` extended attribute; the testkit never reads or
/// writes real attributes.
///
/// - A BOM always decides; a hint can't contradict it.
/// - Otherwise a UTF-8 or Windows-1252 hint is **always honoured**, whatever
///   the bytes. Invalid bytes under a UTF-8 hint are reported as
///   `invalid_encoding` in the usual way: pass the result to
///   [`crate::diagnostics::derive`], which checks UTF-8 validity whenever the
///   encoding is UTF-8.
/// - Another single-byte hint is honoured only if every byte is a
///   character in it ([`decodes`]): some leave bytes unassigned.
/// - A UTF-16 hint on a file without a UTF-16 BOM is ignored.
/// - With no hint, the guess applies ([`expected_encoding`]).
#[must_use]
pub fn reopen_encoding(bytes: &[u8], hint: Option<Encoding>) -> Encoding {
    if Bom::detect(bytes) != Bom::None {
        return expected_encoding(bytes);
    }
    match hint {
        Some(h @ (Encoding::Utf8 | Encoding::Windows1252)) => h,
        Some(h) if h.is_single_byte() && decodes(bytes, h) => h,
        _ => expected_encoding(bytes),
    }
}

/// Whether every byte of `bytes` is a character in `encoding`: false if
/// `encoding` is a single-byte encoding that leaves one of them unassigned
/// (for example 0xAA in Windows-1253).
#[must_use]
pub fn decodes(bytes: &[u8], encoding: Encoding) -> bool {
    let unassigned = unassigned_bytes(encoding);
    !bytes.iter().any(|&b| unassigned[usize::from(b)])
}

/// For each byte value, whether `encoding`, a single-byte encoding, leaves
/// it unassigned (it decodes as U+FFFD). All false for any other encoding.
#[must_use]
pub fn unassigned_bytes(encoding: Encoding) -> [bool; 256] {
    let mut unassigned = [false; 256];
    if let Some(whatwg) = encoding.whatwg() {
        for (b, slot) in (0..=u8::MAX).zip(unassigned.iter_mut()) {
            *slot = whatwg
                .decode_without_bom_handling_and_without_replacement(&[b])
                .is_none();
        }
    }
    unassigned
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
/// invalid sequences as U+FFFD, or a single-byte encoding with any byte it
/// leaves unassigned as U+FFFD. For UTF-16 the bytes are taken as UTF-8,
/// because the testkit stores UTF-16 values transcoded (see
/// [`crate::layout::FieldLayout::value`]).
#[must_use]
pub fn decode_value(bytes: &[u8], encoding: Encoding) -> String {
    match encoding {
        Encoding::Windows1252 => decode_windows_1252(bytes),
        Encoding::Iso8859_1 => bytes.iter().map(|&b| char::from(b)).collect(),
        other => match other.whatwg() {
            Some(whatwg) => whatwg.decode_without_bom_handling(bytes).0.into_owned(),
            None => String::from_utf8_lossy(bytes).into_owned(),
        },
    }
}

/// The text `raw` (bytes as a file in `encoding` holds them) stands for,
/// for Save As UTF-8 (ADR-0008 decision 7), or `None` if any of it isn't
/// text there: invalid UTF-8, a byte a single-byte encoding leaves
/// unassigned, an unpaired surrogate or a final odd byte in UTF-16. Unlike
/// [`decode_value`], `raw` is the file's own bytes (UTF-16 included), and
/// nothing is replaced.
#[must_use]
pub fn decode_strict(raw: &[u8], encoding: Encoding) -> Option<String> {
    match encoding {
        Encoding::Utf8 => std::str::from_utf8(raw).ok().map(str::to_owned),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let (pairs, odd) = raw.as_chunks::<2>();
            if !odd.is_empty() {
                return None;
            }
            let units = pairs.iter().map(|&pair| {
                if encoding == Encoding::Utf16Le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }
            });
            char::decode_utf16(units)
                .collect::<Result<String, _>>()
                .ok()
        }
        Encoding::Windows1252 | Encoding::Iso8859_1 => Some(decode_value(raw, encoding)),
        other => other
            .whatwg()?
            .decode_without_bom_handling_and_without_replacement(raw)
            .map(std::borrow::Cow::into_owned),
    }
}

/// Encodes `text` in `encoding` for writing into a file (DESIGN §3.7).
///
/// # Errors
///
/// Returns the first character the encoding cannot represent. UTF-16 files
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
        Encoding::Iso8859_1 => text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).map_err(|_| c))
            .collect(),
        Encoding::Utf16Le | Encoding::Utf16Be => Err(text.chars().next().unwrap_or('\0')),
        other => {
            let first = text.chars().next().unwrap_or('\0');
            let Some(whatwg) = other.whatwg() else {
                return Err(first);
            };
            // The encoder, not the decoder's table inverted (which is how
            // leal-core encodes), so the two check each other.
            let mut encoder = whatwg.new_encoder();
            // A single-byte encoding writes at most one byte a character.
            let mut out = vec![0; text.len()];
            let (result, _, written) =
                encoder.encode_from_utf8_without_replacement(text, &mut out, true);
            match result {
                encoding_rs::EncoderResult::InputEmpty => {
                    out.truncate(written);
                    Ok(out)
                }
                encoding_rs::EncoderResult::Unmappable(c) => Err(c),
                encoding_rs::EncoderResult::OutputFull => Err(first),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_beats_the_guess_but_not_a_bom() {
        let ascii = b"a,b\n";
        assert_eq!(reopen_encoding(ascii, None), Encoding::Utf8);
        assert_eq!(
            reopen_encoding(ascii, Some(Encoding::Windows1252)),
            Encoding::Windows1252
        );
        // "é" in UTF-8 is "Ã©" in Windows-1252; the hint decides which.
        let e = "é".as_bytes();
        assert_eq!(
            reopen_encoding(e, Some(Encoding::Windows1252)),
            Encoding::Windows1252
        );
        // A UTF-8 hint is honoured even when the bytes aren't valid UTF-8
        // (the guess would say Windows-1252); they get invalid_encoding.
        assert_eq!(expected_encoding(b"\xE9"), Encoding::Windows1252);
        assert_eq!(
            reopen_encoding(b"\xE9", Some(Encoding::Utf8)),
            Encoding::Utf8
        );
        assert_eq!(
            reopen_encoding(b"\xFF\xFF\xFFa", Some(Encoding::Utf8)),
            Encoding::Utf8
        );
        // A BOM wins, and a UTF-16 hint without a BOM is ignored.
        assert_eq!(
            reopen_encoding(b"\xEF\xBB\xBFa", Some(Encoding::Windows1252)),
            Encoding::Utf8
        );
        assert_eq!(
            reopen_encoding(ascii, Some(Encoding::Utf16Le)),
            Encoding::Utf8
        );
    }

    /// Every single-byte encoding: each byte it assigns decodes to a
    /// character that encodes back to that byte and to no other, and each
    /// byte it leaves unassigned reads as U+FFFD, isn't text, and makes a
    /// hint for it not hold.
    #[test]
    fn single_byte_encodings_round_trip_every_assigned_byte() {
        let mut unassigned_somewhere = 0;
        for encoding in Encoding::SINGLE_BYTE {
            let unassigned = unassigned_bytes(encoding);
            let mut seen = std::collections::HashSet::new();
            for b in 0..=u8::MAX {
                let text = decode_value(&[b], encoding);
                if unassigned[usize::from(b)] {
                    unassigned_somewhere += 1;
                    assert_eq!(text, "\u{FFFD}", "{encoding:?} {b:#04x}");
                    assert_eq!(decode_strict(&[b], encoding), None);
                    assert!(!decodes(&[b'a', b], encoding));
                    assert_eq!(
                        reopen_encoding(&[b'a', b], Some(encoding)),
                        expected_encoding(&[b'a', b])
                    );
                    continue;
                }
                assert_eq!(text.chars().count(), 1, "{encoding:?} {b:#04x}");
                assert!(seen.insert(text.clone()), "{encoding:?} {b:#04x} twice");
                assert_eq!(decode_strict(&[b], encoding).as_deref(), Some(&*text));
                assert_eq!(
                    encode_value(&text, encoding),
                    Ok(vec![b]),
                    "{encoding:?} {b:#04x}"
                );
                if b < 0x80 {
                    assert_eq!(text, char::from(b).to_string(), "{encoding:?}: ASCII");
                }
            }
            assert_eq!(encode_value("\u{FFFD}", encoding), Err('\u{FFFD}'));
            assert_eq!(encode_value("a😀", encoding), Err('😀'));
            assert!(encoding.is_single_byte() && encoding.is_ascii_compatible());
        }
        // Windows-1253, 1255 and 1257, among others, leave some bytes
        // unassigned.
        assert!(unassigned_somewhere > 10, "{unassigned_somewhere}");
        assert!(unassigned_bytes(Encoding::Windows1253)[0xAA]);
        assert!(!unassigned_bytes(Encoding::Windows1252).contains(&true));
        assert!(!unassigned_bytes(Encoding::Iso8859_1).contains(&true));
        // Spot checks against the code charts.
        assert_eq!(decode_value(b"\xC0", Encoding::Windows1251), "А");
        assert_eq!(decode_value(b"\xDB", Encoding::MacRoman), "€");
        assert_eq!(decode_value(b"\xA4", Encoding::Iso8859_15), "€");
        assert_eq!(decode_value(b"\x80", Encoding::Iso8859_1), "\u{80}");
        assert_eq!(encode_value("Ł", Encoding::Windows1250), Ok(vec![0xA3]));
        assert_eq!(encode_value("é", Encoding::Windows1251), Err('é'));
        // A hint for an encoding the bytes decode in holds.
        assert_eq!(
            reopen_encoding(b"\xC0,b\n", Some(Encoding::Windows1251)),
            Encoding::Windows1251
        );
    }

    #[test]
    fn utf16_text_is_strict() {
        assert_eq!(
            decode_strict(b"a\0b\0", Encoding::Utf16Le).as_deref(),
            Some("ab")
        );
        assert_eq!(
            decode_strict(b"\0a\0b", Encoding::Utf16Be).as_deref(),
            Some("ab")
        );
        // 😀 as a pair, then each half alone, then an odd byte.
        assert_eq!(
            decode_strict(b"\x3D\xD8\x00\xDE", Encoding::Utf16Le).as_deref(),
            Some("😀")
        );
        assert_eq!(decode_strict(b"\x3D\xD8a\0", Encoding::Utf16Le), None);
        assert_eq!(decode_strict(b"a\0\x00\xDE", Encoding::Utf16Le), None);
        assert_eq!(decode_strict(b"a\0b", Encoding::Utf16Le), None);
        assert_eq!(decode_strict(b"\xFF", Encoding::Utf8), None);
    }

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
    fn ascii_compatible_means_structural_characters_are_their_ascii_byte() {
        let structural = [",", ";", "\t", "|", "\"", "\r", "\n"];
        for (encoding, expected) in [
            (Encoding::Utf8, true),
            (Encoding::Windows1252, true),
            (Encoding::Utf16Le, false),
            (Encoding::Utf16Be, false),
        ] {
            assert_eq!(encoding.is_ascii_compatible(), expected, "{encoding:?}");
            let single_bytes = structural
                .iter()
                .all(|s| encode_value(s, encoding) == Ok(s.as_bytes().to_vec()));
            assert_eq!(single_bytes, expected, "{encoding:?}");
        }
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
