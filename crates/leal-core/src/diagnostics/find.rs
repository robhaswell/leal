//! Which field of one row has a field-level diagnostic (task 1.7).
//!
//! The row marks (`marks.rs`) say which rows have *some* field-level
//! warning, for every row, but not which kind: one bit covers text after a
//! closing quote, invalid encoding, NUL and the unterminated quote. The
//! details popover's **Previous** and **Next** step through one kind,
//! past the report's first [`MAX_LOCATIONS`](super::MAX_LOCATIONS), so a
//! marked row is checked here: its fields are looked at directly, with the
//! same definitions the index pass uses (see [`DiagnosticKind`]).
//!
//! The checks agree with the collector because each kind is decided within
//! one field. A delimiter, quote or line ending is a whole ASCII character
//! (or a whole UTF-16 unit), so no invalid sequence, NUL or surrogate pair
//! spans two fields, and a field's raw bytes alone say whether it has one.

use crate::dialect::Encoding;
use crate::index::CodeUnit;
use crate::rows::{FieldSpan, ParsedRow};

use super::DiagnosticKind;

/// The first field of `row` with an occurrence of `kind`, or `None` if it
/// has none. `bytes` are the file's bytes from offset `base` on, and hold
/// the row. Only for the field-level kinds and the unterminated quote:
/// every other kind gives `None` (ragged rows and info-level kinds are
/// found from the row marks or the report).
pub(crate) fn field_with(
    kind: DiagnosticKind,
    encoding: Encoding,
    bytes: &[u8],
    base: usize,
    row: &ParsedRow,
) -> Option<usize> {
    let raw = |field: &FieldSpan| -> &[u8] {
        let span = field.span();
        span.start
            .checked_sub(base)
            .and_then(|start| bytes.get(start..start + span.len()))
            .unwrap_or_default()
    };
    let has: &dyn Fn(&FieldSpan) -> bool = match kind {
        DiagnosticKind::TextAfterClosingQuote => &|field| field.text_after_quote().is_some(),
        DiagnosticKind::UnterminatedQuote => &|field| field.unterminated(),
        DiagnosticKind::NulBytes => &|field| has_nul(raw(field), encoding),
        DiagnosticKind::InvalidEncoding => &|field| has_invalid(raw(field), encoding),
        DiagnosticKind::RaggedRows
        | DiagnosticKind::MixedLineEndings
        | DiagnosticKind::BlankLines
        | DiagnosticKind::BomPresent => return None,
    };
    row.fields().iter().position(has)
}

/// Whether a row whose bytes (line ending included) are `raw` may have
/// `kind`, judged from the bytes alone, without splitting the row into
/// fields: exactly, for NULs and invalid text (no field boundary splits
/// one, and the line ending is ASCII); for text after a closing quote and
/// the unterminated quote, only whether the row has a quote at all, so the
/// row must then be parsed. `false` for the other kinds.
pub(crate) fn row_may_have(kind: DiagnosticKind, encoding: Encoding, raw: &[u8]) -> bool {
    match kind {
        DiagnosticKind::NulBytes => has_nul(raw, encoding),
        DiagnosticKind::InvalidEncoding => has_invalid(raw, encoding),
        // In UTF-16 the quote unit holds a 0x22 byte too.
        DiagnosticKind::TextAfterClosingQuote | DiagnosticKind::UnterminatedQuote => {
            memchr::memchr(b'"', raw).is_some()
        }
        DiagnosticKind::RaggedRows
        | DiagnosticKind::MixedLineEndings
        | DiagnosticKind::BlankLines
        | DiagnosticKind::BomPresent => false,
    }
}

/// Whether [`row_may_have`]'s answer is final, with no need to parse.
pub(crate) fn decided_by_bytes(kind: DiagnosticKind) -> bool {
    matches!(
        kind,
        DiagnosticKind::NulBytes | DiagnosticKind::InvalidEncoding
    )
}

/// What [`next_hit`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hit {
    /// The first NUL or invalid byte at or after the start is here.
    At(usize),
    /// There is none before `end`.
    None,
    /// The search can't be done on the bytes alone (another encoding), or
    /// was stopped.
    Unsupported,
}

/// How much of the file [`next_hit`] searches between checks of `stop`.
const HIT_WINDOW: usize = 1 << 20;

/// The offset of the first NUL, or of the first invalid UTF-8 byte, in
/// `bytes[from..end]`, for **Next** on a mapped file: one fast search
/// (`memchr`, SIMD UTF-8 validation) instead of looking at each marked row.
/// `from` must start a character (a row's start does). Only UTF-8 and the
/// single-byte encodings for NULs, and UTF-8 for invalid text; anything
/// else is [`Hit::Unsupported`]. `stop` is asked between windows of 1 MiB,
/// and makes it give up with [`Hit::Unsupported`].
pub(crate) fn next_hit(
    kind: DiagnosticKind,
    encoding: Encoding,
    bytes: &[u8],
    from: usize,
    end: usize,
    stop: &dyn Fn() -> bool,
) -> Hit {
    let end = end.min(bytes.len());
    let mut at = from;
    match (kind, encoding.code_unit(), encoding) {
        (DiagnosticKind::NulBytes, CodeUnit::Byte, _) => {
            while at < end {
                if stop() {
                    return Hit::Unsupported;
                }
                let window_end = end.min(at + HIT_WINDOW);
                if let Some(i) = memchr::memchr(0, &bytes[at..window_end]) {
                    return Hit::At(at + i);
                }
                at = window_end;
            }
            Hit::None
        }
        (DiagnosticKind::InvalidEncoding, _, Encoding::Utf8) => {
            while at < end {
                if stop() {
                    return Hit::Unsupported;
                }
                let window_end = end.min(at + HIT_WINDOW);
                match simdutf8::compat::from_utf8(&bytes[at..window_end]) {
                    Ok(_) => at = window_end,
                    // A real error: an invalid sequence, or one cut off at
                    // the end of the search.
                    Err(error) if error.error_len().is_some() || window_end == end => {
                        return Hit::At(at + error.valid_up_to());
                    }
                    // Cut off by the window: go on from that character.
                    Err(error) => at += error.valid_up_to().max(1),
                }
            }
            Hit::None
        }
        _ => Hit::Unsupported,
    }
}

/// Whether `raw` holds a NUL: a 0x00 byte, or in UTF-16 a U+0000 unit
/// (ADR-0003 decision 7). A field starts on a unit boundary.
fn has_nul(raw: &[u8], encoding: Encoding) -> bool {
    match encoding.code_unit() {
        CodeUnit::Byte => memchr::memchr(0, raw).is_some(),
        CodeUnit::Utf16Le | CodeUnit::Utf16Be => raw.as_chunks::<2>().0.contains(&[0, 0]),
    }
}

/// Whether `raw` holds text that doesn't decode in `encoding`, and so
/// displays as U+FFFD: invalid UTF-8, an unpaired surrogate or a final odd
/// byte in UTF-16, or a byte a single-byte encoding doesn't map.
pub(crate) fn has_invalid(raw: &[u8], encoding: Encoding) -> bool {
    match encoding {
        Encoding::Utf8 => simdutf8::basic::from_utf8(raw).is_err(),
        Encoding::Utf16Le => invalid_utf16(raw, u16::from_le_bytes),
        Encoding::Utf16Be => invalid_utf16(raw, u16::from_be_bytes),
        // True Latin-1: every byte is a character.
        Encoding::Iso8859_1 => false,
        other => other.whatwg().is_some_and(|decoder| {
            decoder
                .decode_without_bom_handling_and_without_replacement(raw)
                .is_none()
        }),
    }
}

fn invalid_utf16(raw: &[u8], unit: fn([u8; 2]) -> u16) -> bool {
    let (units, odd) = raw.as_chunks::<2>();
    !odd.is_empty() || char::decode_utf16(units.iter().map(|&pair| unit(pair))).any(|c| c.is_err())
}
