//! Splitting one row into fields. See the module docs in `mod.rs` for the
//! rules.
//!
//! The row's span comes from the index, so it holds no line ending, and
//! every CR or LF in it is inside quotes. That leaves only two things to
//! look for, each with one `memchr` call:
//!
//! - outside quotes, the next delimiter. A quote matters only as a
//!   field's first unit, which is checked directly;
//! - inside quotes, the next quote: `""` is an escaped quote, and any
//!   other quote closes the field.
//!
//! The search reuses the index's [`Units`]: the same code reads bytes
//! (UTF-8 and the single-byte encodings) and UTF-16 code units, so a
//! position is always a byte offset into the file (ADR-0003 decision 6).

use std::ops::Range;

use super::{FieldKind, FieldSpan, ParsedRow};
use crate::index::scan::{Bytes, Units, Utf16};
use crate::index::{CodeUnit, IndexDialect};

/// Splits the row at `span` into fields, reading it from `window`, the
/// file's bytes from offset `base` on (the whole file when `base` is 0).
/// `None` if the span isn't inside the window and after the BOM, or (in
/// UTF-16) doesn't start and end on whole code units.
///
/// The split works in the window's own offsets; the fields are then moved
/// back to offsets in the file.
pub(super) fn parse(
    dialect: IndexDialect,
    window: &[u8],
    base: usize,
    span: Range<usize>,
) -> Option<ParsedRow> {
    let window_end = base.checked_add(window.len())?;
    let fits = dialect.bom_len <= span.start
        && base <= span.start
        && span.start <= span.end
        && span.end <= window_end;
    if !fits {
        return None;
    }
    if dialect.code_unit != CodeUnit::Byte {
        // A row from the index starts on a whole unit, and ends on one or
        // at the end of the file (after a final odd byte), which is the
        // end of any window that holds it.
        let whole = |offset: usize| (offset - dialect.bom_len).is_multiple_of(2);
        if !whole(span.start) || !(whole(span.end) || span.end == window_end) {
            return None;
        }
    }
    let local = span.start - base..span.end - base;
    let mut fields = match dialect.code_unit {
        CodeUnit::Byte => split(&Bytes, window, local, dialect),
        CodeUnit::Utf16Le | CodeUnit::Utf16Be => {
            split(&<Utf16 as Units>::at(dialect, base), window, local, dialect)
        }
    };
    if base > 0 {
        for field in &mut fields {
            *field = field.moved_forward(base);
        }
    }
    Some(ParsedRow { span, fields })
}

/// The fields of the row `span`. Every row has at least one field: a blank
/// row's is empty.
fn split<U: Units>(
    units: &U,
    bytes: &[u8],
    span: Range<usize>,
    dialect: IndexDialect,
) -> Vec<FieldSpan> {
    let w = U::WIDTH;
    let IndexDialect {
        delimiter, quote, ..
    } = dialect;
    let end = span.end;
    let mut fields = Vec::new();
    let mut start = span.start;
    loop {
        let (field_end, kind) = if start + w <= end && units.is(bytes, start, quote) {
            quoted_field(units, bytes, start, end, dialect)
        } else {
            // Unquoted: up to the next delimiter. A quote in it is literal.
            let field_end = units.find1(bytes, start, end, delimiter).unwrap_or(end);
            (field_end, FieldKind::Unquoted)
        };
        fields.push(FieldSpan {
            start,
            len: field_end - start,
            kind,
        });
        if field_end >= end {
            return fields;
        }
        // `field_end` is a delimiter: the next field starts after it.
        start = field_end + w;
    }
}

/// A quoted field that starts at `start`: where it ends (the next
/// delimiter or `end`) and how it is quoted.
fn quoted_field<U: Units>(
    units: &U,
    bytes: &[u8],
    start: usize,
    end: usize,
    dialect: IndexDialect,
) -> (usize, FieldKind) {
    let w = U::WIDTH;
    let IndexDialect {
        delimiter, quote, ..
    } = dialect;
    let closing = if w == 1 {
        closing_quote_in_bytes(bytes, start + w, end, quote)
    } else {
        closing_quote(units, bytes, start + w, end, quote)
    };
    let Some(q) = closing else {
        // The quote never closes. In a row from the index, that means the
        // row runs to the end of the file.
        return (end, FieldKind::Unterminated);
    };
    // The closing quote. Anything between it and the next delimiter is text
    // after the closing quote, where quotes are literal (ADR-0003
    // decision 3).
    let after = q + w;
    let field_end = units.find1(bytes, after, end, delimiter).unwrap_or(end);
    let kind = if field_end > after {
        FieldKind::TextAfterQuote(after)
    } else {
        FieldKind::Quoted
    };
    (field_end, kind)
}

/// The closing quote of a quoted field whose contents start at `from`: the
/// first quote in `from..end` that isn't one of a `""` pair. `None` if the
/// quote never closes. One `memchr` per quote.
fn closing_quote<U: Units>(
    units: &U,
    bytes: &[u8],
    from: usize,
    end: usize,
    quote: u8,
) -> Option<usize> {
    let w = U::WIDTH;
    let mut pos = from;
    loop {
        let q = units.find1(bytes, pos, end, quote)?;
        if q + 2 * w <= end && units.is(bytes, q + w, quote) {
            // `""`: an escaped quote.
            pos = q + 2 * w;
        } else {
            return Some(q);
        }
    }
}

/// [`closing_quote`] for one-byte code units, made fast for fields with
/// many escaped quotes. A `memchr` call costs a few nanoseconds to set up,
/// which adds up when `""` comes every few bytes (a 1 MB field can have
/// 100,000 of them). So when `memchr` finds a `""`, the 64 bytes from it are
/// turned into a bitmask, one bit per byte that is a quote, and the quotes
/// among them are found by walking the bits. Long stretches without quotes
/// are still skipped by `memchr`, and a quote that closes the field
/// straight away needs no mask.
fn closing_quote_in_bytes(bytes: &[u8], from: usize, end: usize, quote: u8) -> Option<usize> {
    let mut pos = from;
    while pos < end {
        let block = pos + memchr::memchr(quote, &bytes[pos..end])?;
        if !(block + 1 < end && bytes[block + 1] == quote) {
            // The usual case, a field with no `""` left: this quote closes
            // it, and no mask is needed.
            return Some(block);
        }
        let block_end = (block + 64).min(end);
        let mut mask = quote_mask(&bytes[block..block_end], quote);
        pos = block_end;
        while mask != 0 {
            // The lowest set bit is the next quote.
            let q = block + mask.trailing_zeros() as usize;
            if q + 1 < end && bytes[q + 1] == quote {
                // `""`. If its second quote is past this block, carry on
                // after it; otherwise clear both quotes' bits.
                if q + 1 == block_end {
                    pos = q + 2;
                    break;
                }
                mask &= !(0b11 << (q - block));
            } else {
                return Some(q);
            }
        }
    }
    None
}

/// One bit per byte of `block` (1 to 64 bytes): bit `i` is set if byte `i`
/// is `quote`. A plain loop over a fixed-size array, which the compiler
/// turns into vector comparisons.
fn quote_mask(block: &[u8], quote: u8) -> u64 {
    let mut padded = [0u8; 64];
    padded[..block.len()].copy_from_slice(block);
    let mut mask = 0u64;
    for (i, &b) in padded.iter().enumerate() {
        mask |= u64::from(b == quote) << i;
    }
    // Bits past the end of `block` are padding: clear them, in case the
    // quote character is 0.
    mask & (u64::MAX >> (64 - block.len()))
}
