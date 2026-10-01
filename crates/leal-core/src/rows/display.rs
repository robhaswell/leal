//! Display values: from a field's raw bytes to the text the grid shows. See
//! the module docs in `mod.rs` for the rules.
//!
//! Both steps borrow from the file when they can. Unquoting is a narrower
//! slice of the same bytes, and only `""` needs a copy. Decoding UTF-8 that
//! is valid, or ASCII in a single-byte encoding, is a check, not a copy. So
//! for most fields of most files, a display value is a `&str` pointing into
//! the file's mapped bytes, and nothing is allocated.

use std::borrow::Cow;
use std::ops::Range;

use super::{Encoding, FieldKind, FieldSpan, RowParser};
use crate::index::CodeUnit;
use crate::index::scan::{Bytes, Units, Utf16};

impl RowParser {
    /// The field's value before decoding: its bytes without the quotes,
    /// with `""` turned into `"`, in the file's own encoding (for UTF-16,
    /// UTF-16 code units). A field with text after its closing quote keeps
    /// its raw bytes (ADR-0003 decision 2).
    ///
    /// `bytes` must be the file the field was parsed from. With a file too
    /// short to hold the field, the value is empty.
    #[must_use]
    pub fn value_bytes<'a>(&self, bytes: &'a [u8], field: &FieldSpan) -> Cow<'a, [u8]> {
        match value_range(field, self.dialect.code_unit.width()) {
            Some((range, escaped)) if bytes.get(field.span()).is_some() => {
                self.value_in(bytes, range, escaped)
            }
            _ => Cow::Borrowed(&[]),
        }
    }

    /// The value bytes in `range` of the file: unescaped if `escaped`.
    fn value_in<'a>(&self, bytes: &'a [u8], range: Range<usize>, escaped: bool) -> Cow<'a, [u8]> {
        if !escaped {
            return Cow::Borrowed(&bytes[range]);
        }
        let quote = self.dialect.quote;
        match self.dialect.code_unit {
            CodeUnit::Byte => unescape(&Bytes, bytes, range, quote),
            CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
                unescape(&Utf16::new(self.dialect), bytes, range, quote)
            }
        }
    }

    /// The first `max_chars` characters of [`display_value`], and whether
    /// the value has more. The grid shows only the start of a cell, so it
    /// uses this: the work is proportional to `max_chars`, not to the
    /// field's length, which keeps a screen with a 1 MB field inside the
    /// 1 ms budget (DESIGN §3.9). The cell inspector, which shows the whole
    /// value, uses [`display_value`].
    ///
    /// The prefix is whole characters (Unicode scalar values, as `char`
    /// counts them), never part of one. It borrows from `bytes` when
    /// [`display_value`] would.
    ///
    /// [`display_value`]: RowParser::display_value
    #[must_use]
    pub fn display_prefix<'a>(
        &self,
        bytes: &'a [u8],
        field: &FieldSpan,
        max_chars: usize,
    ) -> (Cow<'a, str>, bool) {
        let Some((range, escaped)) = value_range(field, self.dialect.code_unit.width())
            .filter(|_| bytes.get(field.span()).is_some())
        else {
            return (Cow::Borrowed(""), false);
        };
        // A window of the raw bytes that is sure to hold `max_chars + 1`
        // characters if the value has that many. Every character comes from
        // at most 4 bytes of value (a UTF-8 sequence, an invalid sequence
        // shown as one U+FFFD, or a UTF-16 surrogate pair), and unescaping
        // at most halves the bytes (`""` → `"`). The extra character is the
        // look-ahead that decoding the last kept one may need (the rest of
        // a UTF-8 sequence, or the low half of a surrogate pair), and it
        // tells whether there is more. The window is a multiple of 8, so in
        // UTF-16 it ends on a whole unit. Cutting it between the two quotes
        // of `""` is harmless: unescaping keeps the first.
        let window = max_chars.saturating_add(1).saturating_mul(8);
        let mut end = range.start.saturating_add(window).min(range.end);
        if self.encoding == Encoding::Utf8 && end < range.end {
            // Don't cut a UTF-8 sequence: step back (at most 3 bytes) to
            // the start of the character, so that valid text still decodes
            // without a copy. The window still holds `max_chars + 1`
            // characters' worth.
            let limit = end.saturating_sub(3).max(range.start);
            while end > limit && is_continuation(bytes[end]) {
                end -= 1;
            }
        }
        let text = match self.value_in(bytes, range.start..end, escaped) {
            Cow::Borrowed(value) => self.decode(value),
            Cow::Owned(value) => Cow::Owned(self.decode(&value).into_owned()),
        };
        first_chars(text, max_chars)
    }

    /// The text the grid shows for the field: [`value_bytes`], decoded in
    /// the file's encoding. Bytes that aren't valid text show as U+FFFD.
    ///
    /// The result borrows from `bytes` when no copy is needed. `bytes`
    /// must be the file the field was parsed from; with a file too short to
    /// hold the field, the value is empty.
    ///
    /// [`value_bytes`]: RowParser::value_bytes
    #[must_use]
    pub fn display_value<'a>(&self, bytes: &'a [u8], field: &FieldSpan) -> Cow<'a, str> {
        match self.value_bytes(bytes, field) {
            Cow::Borrowed(value) => self.decode(value),
            Cow::Owned(value) => Cow::Owned(self.decode(&value).into_owned()),
        }
    }

    /// Decodes a value in the parser's encoding. The result borrows from
    /// `value` when the bytes are already the text.
    fn decode<'b>(&self, value: &'b [u8]) -> Cow<'b, str> {
        match self.encoding {
            Encoding::Utf8 => String::from_utf8_lossy(value),
            Encoding::Utf16Le => Cow::Owned(decode_utf16(value, u16::from_le_bytes)),
            Encoding::Utf16Be => Cow::Owned(decode_utf16(value, u16::from_be_bytes)),
            Encoding::Iso8859_1 => decode_latin1(value),
            other => match other.single_byte() {
                // Single-byte decoders turn any byte they don't map into
                // U+FFFD, and borrow when the bytes are ASCII.
                Some(decoder) => decoder.decode_without_bom_handling(value).0,
                // Every encoding but the four above has a decoder.
                None => String::from_utf8_lossy(value),
            },
        }
    }
}

/// Where a field's value is in the file, and whether it needs unescaping:
/// the raw bytes for an unquoted field or one with text after its closing
/// quote, otherwise the bytes between the quotes (or after the opening
/// quote of an unterminated field). `w` is the code unit's width.
fn value_range(field: &FieldSpan, w: usize) -> Option<(Range<usize>, bool)> {
    let span = field.span();
    match field.kind {
        FieldKind::Unquoted | FieldKind::TextAfterQuote(_) => Some((span, false)),
        // A closed field ends with its closing quote.
        FieldKind::Quoted => Some((span.start + w..span.end.checked_sub(w)?, true)),
        FieldKind::Unterminated => Some((span.start + w..span.end, true)),
    }
}

/// True for a UTF-8 continuation byte, `10xx_xxxx`: never the first byte of
/// a character.
fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// `text` cut to its first `max_chars` characters, and whether anything was
/// cut. A borrowed `text` stays borrowed.
fn first_chars(text: Cow<'_, str>, max_chars: usize) -> (Cow<'_, str>, bool) {
    let Some((cut, _)) = text.char_indices().nth(max_chars) else {
        return (text, false);
    };
    let text = match text {
        Cow::Borrowed(s) => Cow::Borrowed(&s[..cut]),
        Cow::Owned(mut s) => {
            s.truncate(cut);
            Cow::Owned(s)
        }
    };
    (text, true)
}

/// The units of `range` with every `""` turned into `"`. Borrowed if there
/// is no quote at all.
///
/// Between the quotes of a closed field, and after the opening quote of an
/// unterminated one, quotes only ever come in pairs (a lone quote would
/// have closed the field), so each quote found is the first of a pair.
fn unescape<'a, U: Units>(
    units: &U,
    bytes: &'a [u8],
    range: Range<usize>,
    quote: u8,
) -> Cow<'a, [u8]> {
    let w = U::WIDTH;
    let end = range.end;
    let Some(mut q) = units.find1(bytes, range.start, end, quote) else {
        return Cow::Borrowed(&bytes[range]);
    };
    let mut out = Vec::with_capacity(end - range.start);
    let mut from = range.start;
    loop {
        // Keep everything up to and including the first quote of the pair,
        // and skip the second.
        out.extend_from_slice(&bytes[from..q + w]);
        from = q + w;
        if from + w <= end && units.is(bytes, from, quote) {
            from += w;
        }
        match units.find1(bytes, from, end, quote) {
            Some(next) => q = next,
            None => break,
        }
    }
    out.extend_from_slice(&bytes[from..end]);
    Cow::Owned(out)
}

/// UTF-16 code units as text. An unpaired surrogate (ADR-0003 decision 7)
/// and a final odd byte, which isn't a whole unit, each show as U+FFFD.
fn decode_utf16(value: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let (pairs, odd) = value.as_chunks::<2>();
    let mut text: String = char::decode_utf16(pairs.iter().map(|&pair| unit(pair)))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if !odd.is_empty() {
        text.push(char::REPLACEMENT_CHARACTER);
    }
    text
}

/// ISO-8859-1: every byte is the code point of the same value. Borrowed if
/// the bytes are ASCII.
fn decode_latin1(value: &[u8]) -> Cow<'_, str> {
    if value.is_ascii()
        && let Ok(text) = std::str::from_utf8(value)
    {
        return Cow::Borrowed(text);
    }
    Cow::Owned(value.iter().map(|&b| char::from(b)).collect())
}
