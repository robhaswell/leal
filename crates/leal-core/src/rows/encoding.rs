//! The text encodings display values are decoded from.
//!
//! PROVISIONAL (seam with 1.2): this is a local copy of the encoding set of
//! ADR-0005 decision 5, with the same variant names as the `Encoding` enum
//! on task 1.2's branch (`dialect::Encoding`), which isn't on `main` yet.
//! When 1.3a joins detection to the index and the parser, it should delete
//! this enum and use 1.2's, moving [`Encoding::code_unit`] and the
//! decoder lookup across (1.2 has the same `encoding_rs` table as
//! `Encoding::whatwg`).

use crate::index::CodeUnit;

/// A text encoding Leal can read (DESIGN §3.2, ADR-0005 decision 5).
///
/// UTF-8, UTF-16 with a BOM and Windows-1252 are the ones detection picks.
/// The others come only from the `com.apple.TextEncoding` attribute or
/// **Reopen with encoding…**. Every one except UTF-16 is ASCII-compatible:
/// the delimiter, quote and line-ending bytes can't appear inside a
/// character, so the file can be parsed as bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// UTF-8, with or without a BOM. Invalid bytes display as U+FFFD.
    Utf8,
    /// UTF-16 little-endian (read-only in v1). Unpaired surrogates display
    /// as U+FFFD (ADR-0003 decision 7).
    Utf16Le,
    /// UTF-16 big-endian (read-only in v1).
    Utf16Be,
    /// Windows-1252, the single-byte default (WHATWG: every byte decodes).
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

    /// How the encoding stores the structural characters, which is what
    /// the index and the parser need: UTF-16 code units for UTF-16, single
    /// bytes for every other encoding.
    #[must_use]
    pub const fn code_unit(self) -> CodeUnit {
        match self {
            Encoding::Utf16Le => CodeUnit::Utf16Le,
            Encoding::Utf16Be => CodeUnit::Utf16Be,
            _ => CodeUnit::Byte,
        }
    }

    /// The single-byte decoder for this encoding, or `None` for UTF-8,
    /// UTF-16 (decoded with the standard library) and ISO-8859-1.
    ///
    /// The WHATWG Encoding Standard, which `encoding_rs` implements,
    /// treats the label `iso-8859-1` as Windows-1252, so true Latin-1 is
    /// decoded by hand.
    pub(crate) fn single_byte(self) -> Option<&'static encoding_rs::Encoding> {
        Some(match self {
            Encoding::Windows1252 => encoding_rs::WINDOWS_1252,
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
            Encoding::Utf8 | Encoding::Utf16Le | Encoding::Utf16Be | Encoding::Iso8859_1 => {
                return None;
            }
        })
    }
}
