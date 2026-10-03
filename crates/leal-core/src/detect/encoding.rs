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

    /// The counts of two pieces of a file together. The pieces must be
    /// split where [`chunk_end`] splits them, so no sequence is cut.
    pub(crate) fn plus(self, other: Census) -> Census {
        Census {
            multibyte: self.multibyte + other.multibyte,
            invalid: self.invalid + other.invalid,
        }
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

/// [`Census::of`] a file that arrives in pieces cut anywhere, such as the
/// pieces a save writes (task 2.2): a sequence cut between two pieces is
/// held back until the next one, so the sum is the census of the whole.
#[derive(Clone, Debug, Default)]
pub(crate) struct CensusStream {
    census: Census,
    /// The end of the last piece: the start of a sequence it cut off.
    carry: Vec<u8>,
}

impl CensusStream {
    /// Counts the next piece, in place: only a sequence the last piece
    /// cut off is copied, with the at most three bytes of this one that
    /// finish it.
    pub(crate) fn push(&mut self, piece: &[u8]) {
        let mut piece = piece;
        if !self.carry.is_empty() {
            let take = piece.len().min(3);
            let carried = self.carry.len();
            let mut small = std::mem::take(&mut self.carry);
            small.extend_from_slice(&piece[..take]);
            // How far into `small` the sequences that start in the carry
            // reach: a sequence is at most four bytes, so they end in it.
            let mut reach = 0;
            let mut chunks = small.utf8_chunks().peekable();
            'chunks: while let Some(chunk) = chunks.next() {
                for c in chunk.valid().chars() {
                    if reach >= carried {
                        break 'chunks;
                    }
                    reach += c.len_utf8();
                }
                if reach >= carried {
                    break;
                }
                let invalid = chunk.invalid();
                if chunks.peek().is_none() && take == piece.len() && unfinished(invalid) {
                    // Still cut off: it waits for the next piece.
                    self.census = self.census.plus(Census::of(&small[..reach], false));
                    self.carry = small[reach..].to_vec();
                    return;
                }
                reach += invalid.len();
            }
            self.census = self.census.plus(Census::of(&small[..reach], false));
            piece = &piece[reach.saturating_sub(carried).min(piece.len())..];
        }
        if piece.is_ascii() {
            return;
        }
        // A tail that only the end of the piece makes invalid waits for the
        // next piece: at most three bytes.
        let tail = piece.utf8_chunks().last().map_or(0, |chunk| {
            if unfinished(chunk.invalid()) {
                chunk.invalid().len()
            } else {
                0
            }
        });
        let whole = piece.len() - tail;
        self.census = self.census.plus(Census::of(&piece[..whole], false));
        self.carry = piece[whole..].to_vec();
    }

    /// The census of everything pushed.
    pub(crate) fn finish(self) -> Census {
        self.census.plus(Census::of(&self.carry, false))
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
    let assigned = assigned_bytes(encoding);
    bytes.iter().all(|&b| assigned[usize::from(b)])
}

/// For each byte value, whether it is a character in `encoding`, a
/// single-byte encoding.
pub(crate) fn assigned_bytes(encoding: Encoding) -> [bool; 256] {
    let mut assigned = [true; 256];
    let Some(whatwg) = encoding.whatwg() else {
        return assigned; // ISO-8859-1 assigns every byte
    };
    for (byte, slot) in (0..=u8::MAX).zip(assigned.iter_mut()) {
        let one = [byte];
        *slot = whatwg
            .decode_without_bom_handling_and_without_replacement(&one)
            .is_some();
    }
    assigned
}

/// Where a chunk of about `size` bytes starting at `start` should end, so
/// that chunks can be read one at a time: never inside a UTF-16 code unit,
/// and never inside a UTF-8 sequence that could be valid, so that
/// [`Census::plus`] over the chunks equals the census of the whole.
///
/// A UTF-8 split is moved back over at most three continuation bytes
/// (0x80..=0xBF) to the byte before them: a valid sequence has at most
/// three. If more than three come in a row, no valid sequence ends in the
/// split, so it stays.
pub(crate) fn chunk_end(bytes: &[u8], start: usize, size: usize, encoding: Encoding) -> usize {
    let size = if encoding.is_ascii_compatible() {
        size.max(4)
    } else {
        size.max(2) & !1 // an even number of bytes
    };
    let end = start.saturating_add(size);
    if end >= bytes.len() {
        return bytes.len();
    }
    if !encoding.is_ascii_compatible() {
        return end;
    }
    let continuation = |i: usize| bytes[i] & 0xC0 == 0x80;
    (end - 3..=end)
        .rev()
        .find(|&i| !continuation(i))
        .filter(|&i| i > start)
        .unwrap_or(end)
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

    /// Splitting a file into chunks anywhere `chunk_end` allows gives the
    /// same census as the whole, for every chunk size.
    #[test]
    fn a_chunked_census_equals_the_whole() {
        let samples: [&[u8]; 5] = [
            "aé€😀b".as_bytes(),
            b"\xF0\x9F\x98\x80\x80\x80\x80\x80x\xE2\x82",
            b"\x80\x80\x80\x80\x80\x80",
            b"ab\xC3\xA9\xFF\xF0\x9F\x98\xC3",
            b"\xED\xA0\x80\xC0\xAF\xE2\x82\xAC",
        ];
        for bytes in samples {
            let whole = Census::of(bytes, false);
            for size in 1..=bytes.len() {
                let mut sum = Census::default();
                let mut start = 0;
                while start < bytes.len() {
                    let end = chunk_end(bytes, start, size, Encoding::Utf8);
                    assert!(end > start);
                    sum = sum.plus(Census::of(&bytes[start..end], false));
                    start = end;
                }
                assert_eq!(sum, whole, "{} in chunks of {size}", bytes.escape_ascii());
            }
        }
        // UTF-16 chunks have an even number of bytes.
        assert_eq!(chunk_end(&[0; 10], 0, 5, Encoding::Utf16Le), 4);
        assert_eq!(chunk_end(&[0; 10], 4, 5, Encoding::Utf16Le), 8);
        assert_eq!(chunk_end(&[0; 10], 8, 5, Encoding::Utf16Le), 10);
    }

    /// Pushed in pieces cut anywhere, the census is the whole's.
    #[test]
    fn a_streamed_census_equals_the_whole() {
        let samples: [&[u8]; 4] = [
            "aé€😀b".as_bytes(),
            b"\xF0\x9F\x98\x80\x80\x80x\xE2\x82",
            b"ab\xC3\xA9\xFF\xF0\x9F\x98\xC3",
            b"\xED\xA0\x80\xC0\xAF\xE2\x82\xAC\xE2",
        ];
        for bytes in samples {
            let whole = Census::of(bytes, false);
            for size in 1..=bytes.len() {
                let mut stream = CensusStream::default();
                for piece in bytes.chunks(size) {
                    stream.push(piece);
                }
                assert_eq!(
                    stream.finish(),
                    whole,
                    "{} in pieces of {size}",
                    bytes.escape_ascii()
                );
            }
        }
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
