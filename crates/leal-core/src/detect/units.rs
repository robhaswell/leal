//! A file's text as a sequence of code units, so that one row scanner
//! works for every encoding.
//!
//! In the ASCII-compatible encodings a unit is a byte. In UTF-16 it is a
//! 16-bit code unit. Either way, every structural character (the four
//! delimiters, the quote, CR and LF) is exactly one unit with its ASCII
//! value, and never part of another character, so the scanner never needs
//! to decode.

use std::ops::Range;

use crate::dialect::Encoding;

/// Code units over borrowed bytes. Indexes and ranges count units, not
/// bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Units<'a> {
    bytes: &'a [u8],
    encoding: Encoding,
}

impl<'a> Units<'a> {
    /// Units over `bytes` in `encoding`. For UTF-16 an odd final byte is
    /// not a whole unit and is left out.
    pub(crate) fn new(bytes: &'a [u8], encoding: Encoding) -> Self {
        Units { bytes, encoding }
    }

    /// The encoding the units are in.
    pub(crate) const fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// The number of bytes in one unit.
    pub(crate) const fn width(&self) -> usize {
        if self.encoding.is_ascii_compatible() {
            1
        } else {
            2
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len() / self.width()
    }

    /// The unit at `i`, which must be less than [`Units::len`]; past the
    /// end it reads as 0, which is no structural character.
    pub(crate) fn get(&self, i: usize) -> u16 {
        match self.encoding {
            Encoding::Utf16Le => self.pair(i).map_or(0, u16::from_le_bytes),
            Encoding::Utf16Be => self.pair(i).map_or(0, u16::from_be_bytes),
            _ => self.bytes.get(i).map_or(0, |&b| u16::from(b)),
        }
    }

    fn pair(&self, i: usize) -> Option<[u8; 2]> {
        let at = i.checked_mul(2)?;
        match self.bytes.get(at..at.checked_add(2)?)? {
            &[a, b] => Some([a, b]),
            _ => None,
        }
    }

    /// The bytes of the units in `range`.
    pub(crate) fn bytes(&self, range: Range<usize>) -> &'a [u8] {
        let w = self.width();
        let start = range.start.saturating_mul(w).min(self.bytes.len());
        let end = range.end.saturating_mul(w).clamp(start, self.bytes.len());
        &self.bytes[start..end]
    }

    /// The units from `start` to the end.
    pub(crate) fn starting_at(&self, start: usize) -> Units<'a> {
        Units::new(self.bytes(start..self.len()), self.encoding)
    }

    /// The text of the units in `range`, with anything that doesn't decode
    /// shown as U+FFFD.
    pub(crate) fn text(&self, range: Range<usize>) -> String {
        decode(self.bytes(range), self.encoding)
    }
}

/// Decodes `bytes` for display. Invalid sequences, unpaired surrogates and
/// unassigned bytes become U+FFFD.
pub(crate) fn decode(bytes: &[u8], encoding: Encoding) -> String {
    match encoding.whatwg() {
        Some(e) => e.decode_without_bom_handling(bytes).0.into_owned(),
        // ISO-8859-1: each byte is the code point of the same value.
        None => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_units_are_read_in_their_byte_order() {
        let le = Units::new(b"a\0,\0\x3D\xD8\0\xDE\n", Encoding::Utf16Le);
        assert_eq!(le.len(), 4); // the odd final byte is not a unit
        assert_eq!(le.get(0), u16::from(b'a'));
        assert_eq!(le.get(1), u16::from(b','));
        assert_eq!(le.text(2..4), "😀");
        assert_eq!(le.get(4), 0);
        let be = Units::new(b"\0a\0,", Encoding::Utf16Be);
        assert_eq!((be.len(), be.get(0), be.get(1)), (2, 97, 44));
        assert_eq!(be.bytes(1..2), b"\0,");
    }

    #[test]
    fn single_byte_units_decode_in_their_encoding() {
        let bytes = b"caf\xE9 \x80";
        assert_eq!(
            Units::new(bytes, Encoding::Windows1252).text(0..6),
            "café €"
        );
        assert_eq!(
            Units::new(bytes, Encoding::Iso8859_1).text(0..6),
            "café \u{80}"
        );
        assert_eq!(
            Units::new(bytes, Encoding::Utf8).text(0..6),
            "caf\u{FFFD} \u{FFFD}"
        );
        assert_eq!(
            Units::new(b"\xAA", Encoding::Windows1253).text(0..1),
            "\u{FFFD}"
        );
        // Out-of-range ranges are clamped rather than panicking.
        assert_eq!(Units::new(b"ab", Encoding::Utf8).bytes(1..9), b"b");
    }
}
