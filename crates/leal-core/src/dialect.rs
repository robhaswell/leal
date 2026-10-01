//! The words for how a file is read: its delimiter, quote character, line
//! endings, byte order mark and text encoding (DESIGN §3.2).
//!
//! These are plain values. Deciding which ones a file has is
//! [`crate::detect`]'s job.

use crate::index::CodeUnit;

/// The quote character. Other quote characters are out of scope for v1
/// (DESIGN §3.2).
pub const QUOTE: u8 = b'"';

/// A field delimiter Leal detects (DESIGN §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Delimiter {
    /// `,`, also the default for a file with no delimiter at all.
    Comma,
    /// `;`
    Semicolon,
    /// `\t`
    Tab,
    /// `|`
    Pipe,
}

impl Delimiter {
    /// Every delimiter, in the order detection prefers them when two fit
    /// a file equally well.
    pub const ALL: [Delimiter; 4] = [
        Delimiter::Comma,
        Delimiter::Semicolon,
        Delimiter::Tab,
        Delimiter::Pipe,
    ];

    /// The delimiter's byte (and, in UTF-16, its code unit).
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Delimiter::Comma => b',',
            Delimiter::Semicolon => b';',
            Delimiter::Tab => b'\t',
            Delimiter::Pipe => b'|',
        }
    }

    /// The delimiter whose byte is `byte`, if it is one of the four.
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.byte() == byte)
    }

    /// The delimiter's name in Leal's interpretation attribute
    /// ([`crate::attributes::Interpretation`]): `comma`, `semicolon`, `tab`
    /// or `pipe`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Delimiter::Comma => "comma",
            Delimiter::Semicolon => "semicolon",
            Delimiter::Tab => "tab",
            Delimiter::Pipe => "pipe",
        }
    }

    /// The delimiter called `name` (see [`Delimiter::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.name() == name)
    }
}

/// A line ending. A CR directly followed by LF is always one CRLF.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    Crlf,
    /// `\r` on its own
    Cr,
}

impl LineEnding {
    /// The line ending's bytes in an ASCII-compatible encoding.
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            LineEnding::Lf => b"\n",
            LineEnding::Crlf => b"\r\n",
            LineEnding::Cr => b"\r",
        }
    }
}

/// A byte order mark at the very start of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bom {
    /// No BOM.
    None,
    /// `EF BB BF`
    Utf8,
    /// `FF FE`
    Utf16Le,
    /// `FE FF`
    Utf16Be,
}

impl Bom {
    /// The BOM's bytes (empty for [`Bom::None`]).
    #[must_use]
    pub const fn bytes(self) -> &'static [u8] {
        match self {
            Bom::None => b"",
            Bom::Utf8 => b"\xEF\xBB\xBF",
            Bom::Utf16Le => b"\xFF\xFE",
            Bom::Utf16Be => b"\xFE\xFF",
        }
    }

    /// The number of bytes the BOM takes (0, 2 or 3).
    #[must_use]
    pub const fn len(self) -> usize {
        self.bytes().len()
    }

    /// True for [`Bom::None`].
    #[must_use]
    pub const fn is_empty(self) -> bool {
        matches!(self, Bom::None)
    }

    /// The BOM `bytes` start with. `FF FE` is always UTF-16 LE: UTF-32 is
    /// not supported.
    ///
    /// ```
    /// use leal_core::dialect::Bom;
    /// assert_eq!(Bom::detect(b"\xEF\xBB\xBFid,name\n"), Bom::Utf8);
    /// assert_eq!(Bom::detect(b"id,name\n"), Bom::None);
    /// ```
    #[must_use]
    pub fn detect(bytes: &[u8]) -> Self {
        match bytes {
            [0xEF, 0xBB, 0xBF, ..] => Bom::Utf8,
            [0xFF, 0xFE, ..] => Bom::Utf16Le,
            [0xFE, 0xFF, ..] => Bom::Utf16Be,
            _ => Bom::None,
        }
    }

    /// The encoding this BOM means, or `None` for [`Bom::None`].
    #[must_use]
    pub const fn encoding(self) -> Option<Encoding> {
        match self {
            Bom::None => None,
            Bom::Utf8 => Some(Encoding::Utf8),
            Bom::Utf16Le => Some(Encoding::Utf16Le),
            Bom::Utf16Be => Some(Encoding::Utf16Be),
        }
    }
}

/// A text encoding Leal can read (DESIGN §3.2, ADR-0005 decision 5).
///
/// Only UTF-8, UTF-16 with a BOM and Windows-1252 are ever detected. The
/// others are used only when the `com.apple.TextEncoding` attribute names
/// them or the user picks them (**Reopen with encoding…**). All of them
/// except UTF-16 are ASCII-compatible: the delimiter, quote and line-ending
/// bytes can't appear inside a character. Multibyte encodings such as
/// Shift_JIS, whose second bytes can equal `|` or `\`, are not supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// UTF-8, with or without a BOM.
    Utf8,
    /// UTF-16 little-endian, only with a BOM (read-only in v1).
    Utf16Le,
    /// UTF-16 big-endian, only with a BOM (read-only in v1).
    Utf16Be,
    /// Windows-1252, the single-byte default.
    Windows1252,
    /// Windows-1250 (Central European).
    Windows1250,
    /// Windows-1251 (Cyrillic).
    Windows1251,
    /// Windows-1253 (Greek).
    Windows1253,
    /// Windows-1254 (Turkish).
    Windows1254,
    /// Windows-1255 (Hebrew).
    Windows1255,
    /// Windows-1256 (Arabic).
    Windows1256,
    /// Windows-1257 (Baltic).
    Windows1257,
    /// Windows-1258 (Vietnamese).
    Windows1258,
    /// ISO-8859-1 (Latin-1): every byte is the code point of the same value.
    Iso8859_1,
    /// ISO-8859-2 (Latin-2).
    Iso8859_2,
    /// ISO-8859-15 (Latin-9).
    Iso8859_15,
    /// Mac Roman.
    MacRoman,
}

impl Encoding {
    /// Every encoding Leal can read.
    pub const ALL: [Encoding; 16] = [
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
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

    /// The encoding's `CFStringEncoding` number, which is how the
    /// `com.apple.TextEncoding` attribute names it (ADR-0005 decision 5).
    /// UTF-16 gives the byte-order-specific numbers.
    #[must_use]
    pub const fn cf_string_encoding(self) -> u32 {
        match self {
            Encoding::Utf8 => 0x0800_0100,
            Encoding::Utf16Le => 0x1400_0100,
            Encoding::Utf16Be => 0x1000_0100,
            Encoding::Windows1252 => 0x0500,
            Encoding::Windows1250 => 0x0501,
            Encoding::Windows1251 => 0x0502,
            Encoding::Windows1253 => 0x0503,
            Encoding::Windows1254 => 0x0504,
            Encoding::Windows1255 => 0x0505,
            Encoding::Windows1256 => 0x0506,
            Encoding::Windows1257 => 0x0507,
            Encoding::Windows1258 => 0x0508,
            Encoding::Iso8859_1 => 0x0201,
            Encoding::Iso8859_2 => 0x0202,
            Encoding::Iso8859_15 => 0x020F,
            Encoding::MacRoman => 0,
        }
    }

    /// The encoding with `CFStringEncoding` number `value`, if Leal reads
    /// it. The generic UTF-16 number (`0x100`, whose byte order comes from
    /// a BOM) is not one; see [`crate::attributes::parse_text_encoding`].
    #[must_use]
    pub fn from_cf_string_encoding(value: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|e| e.cf_string_encoding() == value)
    }

    /// The encoding's IANA charset name, as macOS writes it in the
    /// `com.apple.TextEncoding` attribute (for example `utf-8`,
    /// `windows-1252` or `macintosh`).
    #[must_use]
    pub const fn iana_name(self) -> &'static str {
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

    /// How the encoding stores the structural characters, which is what
    /// the index and the row parser need: UTF-16 code units for UTF-16,
    /// single bytes for every other encoding.
    #[must_use]
    pub const fn code_unit(self) -> CodeUnit {
        match self {
            Encoding::Utf16Le => CodeUnit::Utf16Le,
            Encoding::Utf16Be => CodeUnit::Utf16Be,
            _ => CodeUnit::Byte,
        }
    }

    /// True for every encoding except UTF-16: the structural bytes (`,` `;`
    /// `\t` `|` `"` CR LF) are single ASCII bytes, so the file can be
    /// parsed as bytes.
    #[must_use]
    pub const fn is_ascii_compatible(self) -> bool {
        !matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }

    /// True for the encodings detection can choose by itself: UTF-8, UTF-16
    /// (from its BOM) and Windows-1252. The rest come only from the
    /// attribute or the user.
    #[must_use]
    pub const fn is_detected(self) -> bool {
        matches!(
            self,
            Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be | Encoding::Windows1252
        )
    }

    /// The `encoding_rs` decoder for this encoding, or `None` for
    /// ISO-8859-1. The WHATWG Encoding Standard, which `encoding_rs`
    /// implements, treats the label `iso-8859-1` as Windows-1252, so true
    /// Latin-1 (every byte is the code point of the same value) is decoded
    /// by hand.
    pub(crate) fn whatwg(self) -> Option<&'static encoding_rs::Encoding> {
        Some(match self {
            Encoding::Utf8 => encoding_rs::UTF_8,
            Encoding::Utf16Le => encoding_rs::UTF_16LE,
            Encoding::Utf16Be => encoding_rs::UTF_16BE,
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
            Encoding::Windows1250 => encoding_rs::WINDOWS_1250,
            Encoding::Windows1251 => encoding_rs::WINDOWS_1251,
            Encoding::Windows1253 => encoding_rs::WINDOWS_1253,
            Encoding::Windows1254 => encoding_rs::WINDOWS_1254,
            Encoding::Windows1255 => encoding_rs::WINDOWS_1255,
            Encoding::Windows1256 => encoding_rs::WINDOWS_1256,
            Encoding::Windows1257 => encoding_rs::WINDOWS_1257,
            Encoding::Windows1258 => encoding_rs::WINDOWS_1258,
            Encoding::Iso8859_1 => return None,
            Encoding::Iso8859_2 => encoding_rs::ISO_8859_2,
            Encoding::Iso8859_15 => encoding_rs::ISO_8859_15,
            Encoding::MacRoman => encoding_rs::MACINTOSH,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimiters_round_trip_through_bytes_and_names() {
        for d in Delimiter::ALL {
            assert_eq!(Delimiter::from_byte(d.byte()), Some(d));
            assert_eq!(Delimiter::from_name(d.name()), Some(d));
        }
        assert_eq!(Delimiter::from_byte(b':'), None);
        assert_eq!(Delimiter::from_name("Comma"), None);
    }

    #[test]
    fn boms_are_recognised_and_name_their_encoding() {
        assert_eq!(Bom::detect(b"\xEF\xBB\xBFa"), Bom::Utf8);
        assert_eq!(Bom::detect(b"\xFF\xFEa\0"), Bom::Utf16Le);
        assert_eq!(Bom::detect(b"\xFE\xFF\0a"), Bom::Utf16Be);
        assert_eq!(Bom::detect(b"\xEF\xBB"), Bom::None);
        assert_eq!(Bom::detect(b""), Bom::None);
        for bom in [Bom::None, Bom::Utf8, Bom::Utf16Le, Bom::Utf16Be] {
            assert_eq!(Bom::detect(bom.bytes()), bom);
            assert_eq!(bom.len(), bom.bytes().len());
            assert_eq!(bom.is_empty(), bom == Bom::None);
        }
        assert_eq!(Bom::Utf16Be.encoding(), Some(Encoding::Utf16Be));
        assert_eq!(Bom::None.encoding(), None);
    }

    /// The numbers and names are the ones CoreFoundation uses
    /// (`CFStringBuiltInEncodings`, `CFStringEncodings` and
    /// `CFStringConvertEncodingToIANACharSetName`, checked on macOS 27).
    #[test]
    fn cf_string_encoding_numbers_and_names_match_core_foundation() {
        let expected = [
            (Encoding::Utf8, 134_217_984, "utf-8"),
            (Encoding::Utf16Le, 335_544_576, "utf-16le"),
            (Encoding::Utf16Be, 268_435_712, "utf-16be"),
            (Encoding::Windows1252, 1280, "windows-1252"),
            (Encoding::Windows1250, 1281, "windows-1250"),
            (Encoding::Windows1251, 1282, "windows-1251"),
            (Encoding::Windows1253, 1283, "windows-1253"),
            (Encoding::Windows1254, 1284, "windows-1254"),
            (Encoding::Windows1255, 1285, "windows-1255"),
            (Encoding::Windows1256, 1286, "windows-1256"),
            (Encoding::Windows1257, 1287, "windows-1257"),
            (Encoding::Windows1258, 1288, "windows-1258"),
            (Encoding::Iso8859_1, 513, "iso-8859-1"),
            (Encoding::Iso8859_2, 514, "iso-8859-2"),
            (Encoding::Iso8859_15, 527, "iso-8859-15"),
            (Encoding::MacRoman, 0, "macintosh"),
        ];
        assert_eq!(expected.len(), Encoding::ALL.len());
        for (encoding, number, name) in expected {
            assert_eq!(encoding.cf_string_encoding(), number, "{encoding:?}");
            assert_eq!(encoding.iana_name(), name, "{encoding:?}");
            assert_eq!(Encoding::from_cf_string_encoding(number), Some(encoding));
        }
        // kCFStringEncodingUTF16 (byte order from a BOM), ASCII, Shift_JIS.
        for other in [256, 1536, 2561] {
            assert_eq!(Encoding::from_cf_string_encoding(other), None);
        }
    }

    #[test]
    fn only_utf16_is_not_ascii_compatible_and_four_are_detected() {
        for e in Encoding::ALL {
            let utf16 = matches!(e, Encoding::Utf16Le | Encoding::Utf16Be);
            assert_eq!(e.is_ascii_compatible(), !utf16, "{e:?}");
        }
        let detected: Vec<_> = Encoding::ALL
            .into_iter()
            .filter(|e| e.is_detected())
            .collect();
        assert_eq!(
            detected,
            [
                Encoding::Utf8,
                Encoding::Utf16Le,
                Encoding::Utf16Be,
                Encoding::Windows1252
            ]
        );
    }

    #[test]
    fn whatwg_decoders_have_the_same_names_except_latin_1() {
        for e in Encoding::ALL {
            match e.whatwg() {
                Some(w) => assert_eq!(w.name().to_ascii_lowercase(), e.iana_name(), "{e:?}"),
                None => assert_eq!(e, Encoding::Iso8859_1),
            }
        }
    }
}
