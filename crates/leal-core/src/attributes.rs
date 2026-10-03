//! The extended attributes detection reads: macOS's `com.apple.TextEncoding`
//! and Leal's own `io.github.robhaswell.leal.interpretation`.
//!
//! This module only parses and writes attribute *values*. Reading them from
//! a file is the `source` module's job (task 1.1; the attribute names are
//! [`TEXT_ENCODING_ATTRIBUTE`](crate::source::TEXT_ENCODING_ATTRIBUTE) and
//! [`INTERPRETATION_ATTRIBUTE`](crate::source::INTERPRETATION_ATTRIBUTE)),
//! and writing them on save is task 2.5.
//!
//! # `com.apple.TextEncoding`
//!
//! Written by TextEdit and `NSString`, for example `utf-8;134217984`: an IANA
//! charset name, a semicolon, and the `CFStringEncoding` number in decimal.
//! Leal matches only the number (ADR-0005 decision 5); the name is for
//! people reading the attribute. ASCII (`us-ascii;1536`) is read as UTF-8,
//! of which it is a subset (ADR-0007 decision 2).
//!
//! # `io.github.robhaswell.leal.interpretation`
//!
//! Leal's remembered delimiter and header choice (ADR-0005 decision 1), and
//! the encoding of a tag it wrote itself (ADR-0013 decision 2), for example
//! `v=1;delimiter=semicolon;header=yes;file=1532-9f3c0e1d2b4a5867`:
//!
//! - UTF-8 text made of `key=value` items separated by `;`, with no spaces.
//!   Whitespace and NULs around the whole value are ignored.
//! - The first item is `v=1`, the format version. Any other version, or no
//!   version, makes the value unreadable.
//! - `delimiter` is `comma`, `semicolon`, `tab` or `pipe`.
//! - `header` is `yes` or `no`.
//! - `file` is the [`Fingerprint`] of the file as Leal saved it: its length
//!   in bytes, `-`, and a 64-bit hash of its first 64 KB as 16 lowercase
//!   hexadecimal digits. While the file still matches it, the remembered
//!   choices are used as they are. Once something else has changed the
//!   file, they are used only if the file still parses sensibly with them
//!   (see [`crate::detect`]). This is ADR-0007 decision 1.
//! - `encoding` is the IANA name of the encoding Leal wrote the file in
//!   and also recorded in `com.apple.TextEncoding` (for example
//!   `windows-1253`; see [`Encoding::iana_name`]), when a reopen would
//!   otherwise ignore that tag. While the file matches `file`, the tag is
//!   Leal's own and is honoured even where a byte doesn't decode in it
//!   (ADR-0013 decision 2). Added in task 2.3: older values have none, and
//!   older Leal ignores it as an unknown key.
//! - Any of these may be left out, meaning that part was not remembered.
//!   Other keys made of lowercase letters, digits, `_` and `-` are ignored,
//!   so a later Leal can add some without older ones rejecting the
//!   attribute. Any other key (`" delimiter"`, `Header`), a key given twice,
//!   or an unknown value for one of the keys above makes the value
//!   unreadable, so a garbled attribute is noted rather than half-read.

use crate::detect::FIRST_PAINT_BYTES;
use crate::dialect::{Delimiter, Encoding};

/// `kCFStringEncodingUTF16`: UTF-16 with its byte order taken from a BOM.
const CF_UTF16: u32 = 0x0100;

/// `kCFStringEncodingASCII`, read as UTF-8 (ADR-0007 decision 2).
const CF_ASCII: u32 = 0x0600;

/// Why a `com.apple.TextEncoding` value names no encoding Leal can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextEncodingError {
    /// The value isn't `name;number` with a decimal number.
    Unreadable,
    /// The number is a valid `CFStringEncoding` that Leal doesn't read, for
    /// example Shift_JIS (`2561`).
    Unsupported {
        /// The `CFStringEncoding` number.
        cf_string_encoding: u32,
    },
    /// A UTF-16 encoding. Leal reads UTF-16 only from its BOM, so the
    /// attribute adds nothing: with a UTF-16 BOM the BOM decides, and
    /// without one the attribute is ignored (ADR-0004 decision 11).
    Utf16 {
        /// The `CFStringEncoding` number.
        cf_string_encoding: u32,
    },
}

/// Parses a `com.apple.TextEncoding` value, such as `utf-8;134217984`.
///
/// ```
/// use leal_core::attributes::parse_text_encoding;
/// use leal_core::dialect::Encoding;
///
/// assert_eq!(parse_text_encoding(b"windows-1252;1280"), Ok(Encoding::Windows1252));
/// // Only the number counts.
/// assert_eq!(parse_text_encoding(b"latin1;1280"), Ok(Encoding::Windows1252));
/// ```
///
/// # Errors
///
/// [`TextEncodingError::Unreadable`] if the value isn't a name, `;` and a
/// decimal number; [`TextEncodingError::Utf16`] for a UTF-16 number; and
/// [`TextEncodingError::Unsupported`] for any other number Leal doesn't
/// read.
pub fn parse_text_encoding(value: &[u8]) -> Result<Encoding, TextEncodingError> {
    let text = std::str::from_utf8(value)
        .map_err(|_| TextEncodingError::Unreadable)?
        .trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
    let (_name, number) = text.split_once(';').ok_or(TextEncodingError::Unreadable)?;
    // `u32::from_str` accepts a leading `+`; CoreFoundation never writes one.
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TextEncodingError::Unreadable);
    }
    let cf_string_encoding: u32 = number.parse().map_err(|_| TextEncodingError::Unreadable)?;
    match Encoding::from_cf_string_encoding(cf_string_encoding) {
        Some(Encoding::Utf16Le | Encoding::Utf16Be) => {
            Err(TextEncodingError::Utf16 { cf_string_encoding })
        }
        Some(encoding) => Ok(encoding),
        None if cf_string_encoding == CF_UTF16 => {
            Err(TextEncodingError::Utf16 { cf_string_encoding })
        }
        // ASCII is a subset of UTF-8, so this can't misread an ASCII file.
        None if cf_string_encoding == CF_ASCII => Ok(Encoding::Utf8),
        None => Err(TextEncodingError::Unsupported { cf_string_encoding }),
    }
}

/// The `com.apple.TextEncoding` value for `encoding`, in the form TextEdit
/// writes: for example `windows-1252;1280`.
///
/// ```
/// use leal_core::attributes::text_encoding_value;
/// use leal_core::dialect::Encoding;
///
/// assert_eq!(text_encoding_value(Encoding::Utf8), "utf-8;134217984");
/// ```
#[must_use]
pub fn text_encoding_value(encoding: Encoding) -> String {
    format!("{};{}", encoding.iana_name(), encoding.cf_string_encoding())
}

/// What Leal remembers about how it read a file (ADR-0005 decision 1).
/// `None` means that part isn't remembered and is guessed instead.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Interpretation {
    /// The remembered delimiter.
    pub delimiter: Option<Delimiter>,
    /// Whether the first row is the header row.
    pub header: Option<bool>,
    /// The file these choices were saved with.
    pub file: Option<Fingerprint>,
    /// The encoding of the `com.apple.TextEncoding` tag Leal wrote with
    /// the file, when a reopen would otherwise ignore it (ADR-0013
    /// decision 2).
    pub encoding: Option<Encoding>,
}

/// Identifies the exact bytes Leal saved, cheaply enough to check at first
/// paint: the file's length and an FNV-1a hash of its first
/// [`FIRST_PAINT_BYTES`]. It isn't a security measure, only a way to notice
/// that another program has rewritten the file since.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    /// The file's length in bytes.
    pub length: u64,
    /// The 64-bit FNV-1a hash of the file's first 64 KB.
    pub head_hash: u64,
}

impl Fingerprint {
    /// The fingerprint of `file`. Only its first 64 KB are read.
    #[must_use]
    pub fn of(file: &[u8]) -> Self {
        Self::from_head(file, u64::try_from(file.len()).unwrap_or(u64::MAX))
    }

    /// The fingerprint of a file `length` bytes long that starts with
    /// `head` (at least its first 64 KB, or all of it). Only the first
    /// 64 KB of `head` are read.
    #[must_use]
    pub fn from_head(head: &[u8], length: u64) -> Self {
        const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x0100_0000_01b3;
        let head = &head[..head.len().min(FIRST_PAINT_BYTES)];
        let head_hash = head.iter().fold(OFFSET_BASIS, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(PRIME)
        });
        Fingerprint { length, head_hash }
    }

    /// Parses `<length>-<16 lowercase hex digits>`.
    fn parse(value: &str) -> Option<Self> {
        let (length, hash) = value.split_once('-')?;
        let decimal = !length.is_empty() && length.bytes().all(|b| b.is_ascii_digit());
        let hex = hash.len() == 16
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !(decimal && hex) {
            return None;
        }
        Some(Fingerprint {
            length: length.parse().ok()?,
            head_hash: u64::from_str_radix(hash, 16).ok()?,
        })
    }
}

/// Why an interpretation attribute value couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterpretationError {
    /// What was wrong with it, in English, for logs.
    pub reason: String,
}

impl Interpretation {
    /// Parses an attribute value; the format is in the [module
    /// documentation](self).
    ///
    /// ```
    /// use leal_core::attributes::Interpretation;
    /// use leal_core::dialect::Delimiter;
    ///
    /// let i = Interpretation::parse(b"v=1;delimiter=semicolon;header=yes").unwrap();
    /// assert_eq!(i.delimiter, Some(Delimiter::Semicolon));
    /// assert_eq!(i.header, Some(true));
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an [`InterpretationError`] if the value isn't UTF-8, doesn't
    /// start with `v=1`, has an item that isn't `key=value`, repeats a key,
    /// or gives `delimiter`, `header`, `file` or `encoding` a value not
    /// listed above.
    pub fn parse(value: &[u8]) -> Result<Self, InterpretationError> {
        let fail = |reason: String| Err(InterpretationError { reason });
        let Ok(text) = std::str::from_utf8(value) else {
            return fail("not UTF-8".to_owned());
        };
        let text = text.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\0');
        let mut items = text.split(';');
        match items.next() {
            Some("v=1") => {}
            Some(other) => return fail(format!("expected `v=1` first, found `{other}`")),
            None => return fail("empty".to_owned()),
        }
        let mut seen: Vec<&str> = Vec::new();
        let mut result = Interpretation::default();
        for item in items {
            let Some((key, value)) = item.split_once('=') else {
                return fail(format!("`{item}` is not key=value"));
            };
            let plain =
                |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-';
            if key.is_empty() || !key.bytes().all(plain) {
                return fail(format!("`{key}` is not a key"));
            }
            if seen.contains(&key) || key == "v" {
                return fail(format!("`{key}` is given twice"));
            }
            seen.push(key);
            match key {
                "delimiter" => match Delimiter::from_name(value) {
                    Some(d) => result.delimiter = Some(d),
                    None => return fail(format!("unknown delimiter `{value}`")),
                },
                "header" => match value {
                    "yes" => result.header = Some(true),
                    "no" => result.header = Some(false),
                    _ => return fail(format!("header must be yes or no, not `{value}`")),
                },
                "file" => match Fingerprint::parse(value) {
                    Some(f) => result.file = Some(f),
                    None => return fail(format!("`{value}` is not a file fingerprint")),
                },
                "encoding" => match ascii_compatible_named(value) {
                    Some(e) => result.encoding = Some(e),
                    None => return fail(format!("unknown encoding `{value}`")),
                },
                // A later version's addition; see the module docs.
                _ => {}
            }
        }
        Ok(result)
    }

    /// The attribute value for this interpretation, for example
    /// `v=1;delimiter=tab;header=no`.
    #[must_use]
    pub fn to_attribute_value(&self) -> String {
        let mut value = String::from("v=1");
        if let Some(d) = self.delimiter {
            value.push_str(";delimiter=");
            value.push_str(d.name());
        }
        if let Some(h) = self.header {
            value.push_str(if h { ";header=yes" } else { ";header=no" });
        }
        if let Some(f) = self.file {
            value.push_str(&format!(";file={}-{:016x}", f.length, f.head_hash));
        }
        if let Some(e) = self.encoding {
            value.push_str(";encoding=");
            value.push_str(e.iana_name());
        }
        value
    }
}

/// The ASCII-compatible encoding (any but UTF-16) whose IANA name is
/// `name`, exactly as [`Encoding::iana_name`] writes it.
fn ascii_compatible_named(name: &str) -> Option<Encoding> {
    Encoding::ALL
        .into_iter()
        .find(|e| e.is_ascii_compatible() && e.iana_name() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_encoding_values_as_macos_writes_them() {
        assert_eq!(parse_text_encoding(b"utf-8;134217984"), Ok(Encoding::Utf8));
        assert_eq!(parse_text_encoding(b"macintosh;0"), Ok(Encoding::MacRoman));
        assert_eq!(
            parse_text_encoding(b"iso-8859-15;527"),
            Ok(Encoding::Iso8859_15)
        );
        // The number decides, whatever the name says.
        assert_eq!(
            parse_text_encoding(b"utf-8;1280"),
            Ok(Encoding::Windows1252)
        );
        assert_eq!(parse_text_encoding(b";1281"), Ok(Encoding::Windows1250));
        // Surrounding whitespace or a NUL terminator is tolerated.
        assert_eq!(
            parse_text_encoding(b" utf-8;134217984\n\0"),
            Ok(Encoding::Utf8)
        );
    }

    #[test]
    fn every_encoding_round_trips_except_utf16() {
        for e in Encoding::ALL {
            let parsed = parse_text_encoding(text_encoding_value(e).as_bytes());
            if e.is_ascii_compatible() {
                assert_eq!(parsed, Ok(e));
            } else {
                assert_eq!(
                    parsed,
                    Err(TextEncodingError::Utf16 {
                        cf_string_encoding: e.cf_string_encoding()
                    })
                );
            }
        }
        assert_eq!(
            text_encoding_value(Encoding::Windows1252),
            "windows-1252;1280"
        );
    }

    #[test]
    fn utf16_and_unsupported_numbers_are_told_apart() {
        for n in [256u32, 268_435_712, 335_544_576] {
            assert_eq!(
                parse_text_encoding(format!("utf-16;{n}").as_bytes()),
                Err(TextEncodingError::Utf16 {
                    cf_string_encoding: n
                })
            );
        }
        for n in [2561u32, 201_326_848] {
            assert_eq!(
                parse_text_encoding(format!("x;{n}").as_bytes()),
                Err(TextEncodingError::Unsupported {
                    cf_string_encoding: n
                })
            );
        }
    }

    /// ADR-0007 decision 2.
    #[test]
    fn an_ascii_attribute_reads_as_utf8() {
        assert_eq!(parse_text_encoding(b"us-ascii;1536"), Ok(Encoding::Utf8));
    }

    #[test]
    fn malformed_text_encoding_values_are_unreadable() {
        for bad in [
            &b""[..],
            b"utf-8",
            b"utf-8;",
            b"utf-8;+1280",
            b"utf-8;-1",
            b"utf-8;12x",
            b"utf-8;99999999999",
            b"utf-8; 1280",
            b"\xFF;1280",
        ] {
            assert_eq!(
                parse_text_encoding(bad),
                Err(TextEncodingError::Unreadable),
                "{}",
                bad.escape_ascii()
            );
        }
    }

    #[test]
    fn interpretations_round_trip() {
        let files = [
            None,
            Some(Fingerprint::of(b"")),
            Some(Fingerprint::of(b"a,b\n")),
        ];
        let encodings = Encoding::ALL
            .into_iter()
            .filter(|e| e.is_ascii_compatible())
            .map(Some)
            .chain([None]);
        for encoding in encodings {
            for delimiter in [None, Some(Delimiter::Comma), Some(Delimiter::Tab)] {
                for header in [None, Some(true), Some(false)] {
                    for file in files {
                        let i = Interpretation {
                            delimiter,
                            header,
                            file,
                            encoding,
                        };
                        assert_eq!(
                            Interpretation::parse(i.to_attribute_value().as_bytes()),
                            Ok(i)
                        );
                    }
                }
            }
        }
        assert_eq!(
            Interpretation {
                delimiter: Some(Delimiter::Pipe),
                header: Some(false),
                file: Some(Fingerprint {
                    length: 5,
                    head_hash: 0xab
                }),
                encoding: None,
            }
            .to_attribute_value(),
            "v=1;delimiter=pipe;header=no;file=5-00000000000000ab"
        );
        assert_eq!(
            Interpretation {
                file: Some(Fingerprint {
                    length: 5,
                    head_hash: 0xab
                }),
                encoding: Some(Encoding::Windows1253),
                ..Interpretation::default()
            }
            .to_attribute_value(),
            "v=1;file=5-00000000000000ab;encoding=windows-1253"
        );
        assert_eq!(Interpretation::default().to_attribute_value(), "v=1");
    }

    #[test]
    fn interpretation_parsing_ignores_unknown_keys_and_order() {
        assert_eq!(
            Interpretation::parse(b"v=1;colour=blue;header=no;delimiter=tab\n"),
            Ok(Interpretation {
                delimiter: Some(Delimiter::Tab),
                header: Some(false),
                file: None,
                encoding: None,
            })
        );
    }

    /// Values written before the `encoding` key (task 2.3) read as before,
    /// with no encoding remembered, and are written back the same; the key
    /// can come in any position.
    #[test]
    fn interpretations_without_an_encoding_still_read() {
        let old = b"v=1;delimiter=semicolon;header=yes;file=1532-9f3c0e1d2b4a5867";
        let read = Interpretation::parse(old).unwrap();
        assert_eq!(read.delimiter, Some(Delimiter::Semicolon));
        assert_eq!(read.encoding, None);
        assert_eq!(read.to_attribute_value().as_bytes(), old);
        assert_eq!(
            Interpretation::parse(b"v=1;encoding=macintosh;header=no")
                .unwrap()
                .encoding,
            Some(Encoding::MacRoman)
        );
    }

    /// FNV-1a 64's published test vectors, and the 64 KB limit.
    #[test]
    fn fingerprints_hash_the_first_64_kb() {
        assert_eq!(Fingerprint::of(b"").head_hash, 0xcbf2_9ce4_8422_2325);
        assert_eq!(Fingerprint::of(b"a").head_hash, 0xaf63_dc4c_8601_ec8c);
        assert_eq!(Fingerprint::of(b"foobar").head_hash, 0x8594_4171_f739_67e8);
        let mut long = vec![b'x'; FIRST_PAINT_BYTES];
        let head = Fingerprint::of(&long);
        long.push(b'y');
        let longer = Fingerprint::of(&long);
        assert_eq!(longer.length, head.length + 1);
        assert_eq!(longer.head_hash, head.head_hash);
    }

    #[test]
    fn malformed_interpretations_are_errors() {
        for bad in [
            &b""[..],
            b"v=2;delimiter=tab",
            b"delimiter=tab",
            b"v=1;delimiter=colon",
            b"v=1;delimiter=;",
            b"v=1;header=true",
            b"v=1;header",
            b"v=1;header=yes;header=no",
            b"v=1;v=1",
            b"v=1;",
            b"v=1 ;header=yes",
            b"v=1; delimiter=tab",
            b"v=1;Header=yes",
            b"v=1;=yes",
            b"v=1;file=12",
            b"v=1;file=12-abc",
            b"v=1;file=12-00000000000000AB",
            b"v=1;file=-00000000000000ab",
            b"v=1;file=x-00000000000000ab",
            b"v=1;encoding=",
            b"v=1;encoding=greek",
            b"v=1;encoding=Windows-1253",
            b"v=1;encoding=1253",
            b"v=1;encoding=utf-16le",
            b"v=1;encoding=windows-1253;encoding=windows-1253",
            b"\xFFv=1",
        ] {
            assert!(
                Interpretation::parse(bad).is_err(),
                "{}",
                bad.escape_ascii()
            );
        }
    }
}
