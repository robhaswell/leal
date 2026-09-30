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

#[cfg(test)]
mod tests {
    use super::*;

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
