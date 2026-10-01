//! The encoding guess (ADR-0003 decision 1) and the check that bytes decode
//! under a single-byte encoding (ADR-0004 decision 11).

use crate::dialect::Encoding;

/// What the encoding guess counts in a BOM-less file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Census {
    /// Valid UTF-8 sequences of two to four bytes.
    pub multibyte: usize,
    /// Bytes that belong to no valid UTF-8 sequence. A truncated sequence
    /// counts every byte it has (`E2 82` is two).
    pub invalid: usize,
}

impl Census {
    /// Counts `bytes`. If `cut`, the bytes are the start of a longer file,
    /// and a sequence that is unfinished only because the bytes stop is not
    /// counted: the rest of it is just past the cut.
    pub(crate) fn of(bytes: &[u8], cut: bool) -> Self {
        let mut census = Census::default();
        if bytes.is_ascii() {
            return census;
        }
        let mut chunks = bytes.utf8_chunks().peekable();
        while let Some(chunk) = chunks.next() {
            // Each multibyte character has exactly one lead byte, 0xC0 or
            // above; continuation bytes are 0x80..=0xBF.
            census.multibyte += chunk.valid().bytes().filter(|&b| b >= 0xC0).count();
            let invalid = chunk.invalid();
            let last = chunks.peek().is_none();
            if !(cut && last && unfinished(invalid)) {
                census.invalid += invalid.len();
            }
        }
        census
    }

    /// The encoding ADR-0003 decision 1 gives a file without a BOM: UTF-8
    /// if it is pure ASCII, or if it has at least one valid multibyte
    /// sequence and more of them than invalid bytes; otherwise
    /// Windows-1252.
    pub(crate) fn guess(self) -> Encoding {
        let ascii = self.multibyte == 0 && self.invalid == 0;
        if ascii || (self.multibyte >= 1 && self.multibyte > self.invalid) {
            Encoding::Utf8
        } else {
            Encoding::Windows1252
        }
    }
}

/// True if `invalid` (the invalid tail of the bytes) is the start of a
/// valid sequence that the end of the bytes cut off, rather than bytes
/// that could never be valid.
fn unfinished(invalid: &[u8]) -> bool {
    !invalid.is_empty() && std::str::from_utf8(invalid).is_err_and(|e| e.error_len().is_none())
}

/// True if every byte of `bytes` is a character in `encoding`, a
/// single-byte encoding. Some have unassigned bytes (for example 0xAA in
/// Windows-1253), which don't decode.
pub(crate) fn decodes(bytes: &[u8], encoding: Encoding) -> bool {
    let Some(whatwg) = encoding.whatwg() else {
        return true; // ISO-8859-1 assigns every byte
    };
    let mut assigned = [true; 256];
    for (byte, slot) in (0..=u8::MAX).zip(assigned.iter_mut()) {
        let one = [byte];
        *slot = whatwg
            .decode_without_bom_handling_and_without_replacement(&one)
            .is_some();
    }
    bytes.iter().all(|&b| assigned[usize::from(b)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn census(bytes: &[u8], cut: bool) -> (usize, usize) {
        let c = Census::of(bytes, cut);
        (c.multibyte, c.invalid)
    }

    #[test]
    fn the_census_counts_sequences_and_invalid_bytes() {
        assert_eq!(census(b"", false), (0, 0));
        assert_eq!(census("aé€😀\u{FEFF}".as_bytes(), false), (4, 0));
        // A truncated "€" counts both its bytes; then "é", then 0xFF.
        assert_eq!(census(b"\xE2\x82x\xC3\xA9\xFF", false), (1, 3));
        // An overlong "/" and an encoded surrogate are invalid.
        assert_eq!(census(b"\xC0\xAF\xED\xA0\x80", false), (0, 5));
    }

    #[test]
    fn a_sequence_cut_by_the_end_of_a_sample_is_not_counted() {
        for tail in [&b"\xF0"[..], b"\xF0\x9F", b"\xF0\x9F\x98"] {
            let bytes = [b"ab\xC3\xA9".as_slice(), tail].concat();
            assert_eq!(census(&bytes, true), (1, 0));
            assert_eq!(census(&bytes, false), (1, tail.len()));
        }
        // A byte that could never start a sequence still counts.
        assert_eq!(census(b"ab\xFF", true), (0, 1));
        // Only the very end is forgiven.
        assert_eq!(census(b"\xF0\x9F a", true), (0, 2));
    }

    #[test]
    fn the_guess_follows_adr_0003() {
        let guess = |b: &[u8]| Census::of(b, false).guess();
        assert_eq!(guess(b""), Encoding::Utf8);
        assert_eq!(guess(b"a,b\n"), Encoding::Utf8);
        assert_eq!(guess(b"\xC3\xA9\xC3\xBC\xE9"), Encoding::Utf8);
        // A tie is not "outnumber".
        assert_eq!(guess(b"\xC3\xA9\xE9"), Encoding::Windows1252);
        assert_eq!(guess(b"Caf\xE9"), Encoding::Windows1252);
    }

    #[test]
    fn single_byte_encodings_decode_unless_a_byte_is_unassigned() {
        let all: Vec<u8> = (0..=u8::MAX).collect();
        for e in [
            Encoding::Windows1252,
            Encoding::Windows1250,
            Encoding::Windows1251,
            Encoding::Iso8859_1,
            Encoding::Iso8859_2,
            Encoding::Iso8859_15,
            Encoding::MacRoman,
        ] {
            assert!(decodes(&all, e), "{e:?}");
        }
        assert!(!decodes(b"a\xAA", Encoding::Windows1253));
        assert!(decodes(b"a\xE9", Encoding::Windows1253));
        assert!(!decodes(b"\xD9", Encoding::Windows1255));
    }
}
