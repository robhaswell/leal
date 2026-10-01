//! Diagnostics (DESIGN §3.5): what counts as one occurrence, and where it is.
//!
//! The units follow ADR-0003 decision 4: per row (ragged rows, blank lines,
//! mixed line endings), per field (text after a closing quote, invalid
//! encoding, NUL bytes) and per file (unterminated quote, BOM).
//!
//! [`derive()`] is the testkit's definition of the expected diagnostics for a
//! [`Layout`]. The hand-written corpus sidecars are checked against it (via the
//! reference parser in this crate's tests), and later tasks can compare the
//! real diagnostics against it for generated files.

use serde::Deserialize;

use crate::dialect::Encoding;
use crate::layout::{Layout, mode};

/// The most locations a diagnostic records (DESIGN §3.5).
pub const MAX_LOCATIONS: usize = 1000;

/// A kind of irregularity. The sidecar spelling is the snake_case name, for
/// example `unterminated_quote`. Each variant documents what one occurrence
/// is and which offset its location points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// Error. One occurrence at most: the quoted field whose opening quote
    /// is never closed. Offset: the opening quote.
    UnterminatedQuote,
    /// Warning. One occurrence per non-blank row whose field count differs
    /// from the most common field count among non-blank rows (ties go to the
    /// count seen first). Blank lines are never ragged. Offset: row start.
    RaggedRows,
    /// Warning. One occurrence per field with bytes between its closing quote
    /// and the next delimiter or line ending. Offset: the first such byte.
    TextAfterClosingQuote,
    /// Warning. One occurrence per field containing invalid text: invalid
    /// UTF-8 in a UTF-8 file (sequences split as `String::from_utf8_lossy`
    /// splits them), or an unpaired surrogate in a UTF-16 file. Never in
    /// Windows-1252. Offset: the first invalid byte (UTF-16: the first byte of
    /// the code unit) in the field.
    InvalidEncoding,
    /// Warning. One occurrence per field containing a NUL: a `0x00` byte, or
    /// in UTF-16 a U+0000 code unit. Offset: the first NUL in the field (its
    /// code unit's first byte in UTF-16).
    NulBytes,
    /// Info. One occurrence per row whose line ending differs from the most
    /// common line ending (ties go to the one seen first). Offset: the first
    /// byte of that row's line ending.
    MixedLineEndings,
    /// Info. One occurrence per row with no bytes before its line ending
    /// (anywhere in the file, including at the end). Offset: row start.
    BlankLines,
    /// Info. One occurrence if the file starts with a BOM. Row 0, offset 0.
    BomPresent,
}

impl DiagnosticKind {
    /// Every kind, in severity order (errors first).
    pub const ALL: [DiagnosticKind; 8] = [
        DiagnosticKind::UnterminatedQuote,
        DiagnosticKind::RaggedRows,
        DiagnosticKind::TextAfterClosingQuote,
        DiagnosticKind::InvalidEncoding,
        DiagnosticKind::NulBytes,
        DiagnosticKind::MixedLineEndings,
        DiagnosticKind::BlankLines,
        DiagnosticKind::BomPresent,
    ];

    /// The kind's severity (DESIGN §3.5).
    #[must_use]
    pub fn severity(self) -> Severity {
        match self {
            DiagnosticKind::UnterminatedQuote => Severity::Error,
            DiagnosticKind::RaggedRows
            | DiagnosticKind::TextAfterClosingQuote
            | DiagnosticKind::InvalidEncoding
            | DiagnosticKind::NulBytes => Severity::Warning,
            DiagnosticKind::MixedLineEndings
            | DiagnosticKind::BlankLines
            | DiagnosticKind::BomPresent => Severity::Info,
        }
    }
}

/// How serious a diagnostic is. Warnings and errors show the banner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Status bar and details view only.
    Info,
    /// Shows the banner.
    Warning,
    /// Shows the banner, prominently.
    Error,
}

/// Where one occurrence is: a 0-based physical row and a 0-based byte offset
/// into the file as stored (including any BOM).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    /// Physical row index.
    pub row: usize,
    /// Byte offset.
    pub offset: usize,
}

/// One kind of irregularity found in a file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// What was found.
    pub kind: DiagnosticKind,
    /// How many occurrences there are in the whole file.
    pub count: usize,
    /// The first locations in file order, at most [`MAX_LOCATIONS`]. A
    /// sidecar may list fewer; see `tests/corpus/README.md`.
    pub first: Vec<Location>,
}

impl Diagnostic {
    /// A diagnostic with every location given (the list is truncated to
    /// [`MAX_LOCATIONS`]). Returns `None` for an empty list.
    #[must_use]
    pub fn from_locations(kind: DiagnosticKind, mut locations: Vec<Location>) -> Option<Self> {
        if locations.is_empty() {
            return None;
        }
        let count = locations.len();
        locations.truncate(MAX_LOCATIONS);
        Some(Diagnostic {
            kind,
            count,
            first: locations,
        })
    }
}

/// The offsets of invalid UTF-8 sequences in `bytes`, one per replacement
/// character that `String::from_utf8_lossy` would produce.
#[must_use]
pub fn invalid_utf8_offsets(bytes: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut pos = 0;
    while pos < bytes.len() {
        match std::str::from_utf8(&bytes[pos..]) {
            Ok(_) => break,
            Err(e) => {
                let start = pos + e.valid_up_to();
                offsets.push(start);
                // `None` means an incomplete sequence at the end of the input.
                pos = start + e.error_len().unwrap_or(bytes.len() - start);
            }
        }
    }
    offsets
}

/// Offsets of U+0000 code units and of unpaired surrogates in UTF-16 text
/// that starts after a `bom_len`-byte BOM. Each offset is the code unit's
/// first byte (ADR-0003 decision 7). A final odd byte is an incomplete code
/// unit and counts as invalid.
#[must_use]
pub fn utf16_nul_and_invalid_offsets(
    bytes: &[u8],
    bom_len: usize,
    little_endian: bool,
) -> (Vec<usize>, Vec<usize>) {
    let unit_at = |offset: usize| {
        let pair = [bytes[offset], bytes[offset + 1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let (mut nuls, mut invalid) = (Vec::new(), Vec::new());
    let mut pos = bom_len;
    while pos + 1 < bytes.len() {
        let unit = unit_at(pos);
        match unit {
            0 => nuls.push(pos),
            0xD800..=0xDBFF => {
                let next_is_low =
                    pos + 3 < bytes.len() && (0xDC00..=0xDFFF).contains(&unit_at(pos + 2));
                if next_is_low {
                    pos += 4; // a valid pair
                    continue;
                }
                invalid.push(pos);
            }
            0xDC00..=0xDFFF => invalid.push(pos),
            _ => {}
        }
        pos += 2;
    }
    if pos < bytes.len() {
        invalid.push(pos);
    }
    (nuls, invalid)
}

/// The diagnostics a parser should report for `bytes` in `encoding`, given
/// their layout (with file offsets). The result is sorted by kind, in
/// [`DiagnosticKind::ALL`] order.
///
/// - UTF-8: NUL is a 0x00 byte; invalid encoding is invalid UTF-8.
/// - Windows-1252: NUL is a 0x00 byte; every byte is valid.
/// - UTF-16: NUL is a U+0000 code unit; invalid encoding is an unpaired
///   surrogate (ADR-0003 decision 7) or a final odd byte.
#[must_use]
pub fn derive(layout: &Layout, bytes: &[u8], encoding: Encoding) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |kind, locations| {
        if let Some(d) = Diagnostic::from_locations(kind, locations) {
            out.push(d);
        }
    };

    let fields = || {
        layout
            .rows
            .iter()
            .enumerate()
            .flat_map(|(ri, r)| r.fields.iter().map(move |f| (ri, f)))
    };

    push(
        DiagnosticKind::UnterminatedQuote,
        fields()
            .filter(|(_, f)| f.unterminated)
            .map(|(row, f)| Location {
                row,
                offset: f.span.start,
            })
            .collect(),
    );

    let field_mode = layout.field_count_mode();
    push(
        DiagnosticKind::RaggedRows,
        layout
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.is_blank() && Some(r.fields.len()) != field_mode)
            .map(|(row, r)| Location {
                row,
                offset: r.span.start,
            })
            .collect(),
    );

    push(
        DiagnosticKind::TextAfterClosingQuote,
        fields()
            .filter_map(|(row, f)| f.text_after_quote.map(|offset| Location { row, offset }))
            .collect(),
    );

    let (nuls, invalid): (Vec<usize>, Vec<usize>) = match encoding {
        Encoding::Utf8 => (
            positions_of(0, bytes).collect(),
            invalid_utf8_offsets(bytes),
        ),
        Encoding::Windows1252 => (positions_of(0, bytes).collect(), Vec::new()),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            utf16_nul_and_invalid_offsets(bytes, layout.bom_len, encoding == Encoding::Utf16Le)
        }
    };
    push(
        DiagnosticKind::InvalidEncoding,
        first_per_field(layout, invalid.into_iter()),
    );
    push(
        DiagnosticKind::NulBytes,
        first_per_field(layout, nuls.into_iter()),
    );

    let dominant = mode(layout.rows.iter().filter_map(|r| r.line_ending));
    push(
        DiagnosticKind::MixedLineEndings,
        layout
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.line_ending.is_some() && r.line_ending != dominant)
            .map(|(row, r)| Location {
                row,
                offset: r.span.end,
            })
            .collect(),
    );

    push(
        DiagnosticKind::BlankLines,
        layout
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_blank())
            .map(|(row, r)| Location {
                row,
                offset: r.span.start,
            })
            .collect(),
    );

    if layout.bom_len > 0 {
        push(
            DiagnosticKind::BomPresent,
            vec![Location { row: 0, offset: 0 }],
        );
    }

    out
}

/// One location per field: the first of `offsets` (which must be in
/// increasing order) that falls in each field. An offset outside every field
/// (which a structural ASCII byte can't produce) counts on its own.
fn first_per_field(layout: &Layout, offsets: impl Iterator<Item = usize>) -> Vec<Location> {
    let mut out = Vec::new();
    let mut last_field = None;
    for offset in offsets {
        let field = layout.field_of_offset(offset);
        if field.is_some() && field == last_field {
            continue;
        }
        last_field = field;
        let row = field.map_or_else(|| layout.row_of_offset(offset).unwrap_or(0), |(r, _)| r);
        out.push(Location { row, offset });
    }
    out
}

fn positions_of(needle: u8, haystack: &[u8]) -> impl Iterator<Item = usize> + '_ {
    haystack
        .iter()
        .enumerate()
        .filter(move |&(_, b)| *b == needle)
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_utf8_offsets_match_from_utf8_lossy() {
        let cases: &[&[u8]] = &[
            b"",
            b"plain",
            b"\xFF",
            b"a\xC3,b",
            b"\xE2\x82",      // truncated at end
            b"\xE2\x82x\xE2", // truncated, then truncated at end
            b"\xED\xA0\x80",  // surrogate: three replacements
            b"\xC0\xAF",      // overlong: two replacements
            "é€😀".as_bytes(),
        ];
        for bytes in cases {
            let lossy = String::from_utf8_lossy(bytes);
            let expected = lossy.chars().filter(|&c| c == '\u{FFFD}').count();
            assert_eq!(invalid_utf8_offsets(bytes).len(), expected, "{bytes:?}");
        }
        assert_eq!(invalid_utf8_offsets(b"a\xC3,b\xFF"), vec![1, 4]);
    }

    #[test]
    fn nul_and_invalid_bytes_count_once_per_field() {
        use crate::dialect::Delimiter;
        // A hand-built layout: `\0x\0,\xFF\xFE\n\0` has fields 0..3 and 4..6
        // in row 0, then 7..8 in row 1.
        let bytes = b"\0x\0,\xFF\xFE\n\0";
        let field = |span: std::ops::Range<usize>| crate::layout::FieldLayout {
            value: bytes[span.clone()].to_vec(),
            span,
            quoted: false,
            text_after_quote: None,
            unterminated: false,
        };
        let layout = Layout {
            bom_len: 0,
            rows: vec![
                crate::layout::RowLayout {
                    span: 0..6,
                    line_ending: Some(crate::dialect::LineEnding::Lf),
                    fields: vec![field(0..3), field(4..6)],
                },
                crate::layout::RowLayout {
                    span: 7..8,
                    line_ending: None,
                    fields: vec![field(7..8)],
                },
            ],
        };
        assert_eq!(layout.check_tiles(bytes, Delimiter::Comma), Ok(()));
        let d = derive(&layout, bytes, Encoding::Utf8);
        let nul = d
            .iter()
            .find(|d| d.kind == DiagnosticKind::NulBytes)
            .unwrap();
        assert_eq!(nul.count, 2);
        assert_eq!(
            nul.first,
            vec![
                Location { row: 0, offset: 0 },
                Location { row: 1, offset: 7 }
            ]
        );
        let bad = d
            .iter()
            .find(|d| d.kind == DiagnosticKind::InvalidEncoding)
            .unwrap();
        assert_eq!(bad.count, 1);
        assert_eq!(bad.first, vec![Location { row: 0, offset: 4 }]);
        assert!(
            derive(&layout, bytes, Encoding::Windows1252)
                .iter()
                .all(|d| d.kind != DiagnosticKind::InvalidEncoding)
        );
    }

    #[test]
    fn utf16_nuls_are_units_and_lone_surrogates_are_invalid() {
        // BOM, "a", U+0000, lone high surrogate, "b", a valid pair (😀),
        // lone low surrogate, then one stray byte.
        let le: Vec<u8> = [
            0xFEFF_u16, 0x61, 0x0000, 0xD800, 0x62, 0xD83D, 0xDE00, 0xDC00,
        ]
        .iter()
        .flat_map(|u| u.to_le_bytes())
        .chain([0x41])
        .collect();
        let (nuls, invalid) = utf16_nul_and_invalid_offsets(&le, 2, true);
        // "a" is 61 00: its 0x00 byte is not a NUL unit.
        assert_eq!(nuls, vec![4]);
        assert_eq!(invalid, vec![6, 14, 16]);

        let be: Vec<u8> = [0xFEFF_u16, 0x0000, 0xDBFF]
            .iter()
            .flat_map(|u| u.to_be_bytes())
            .collect();
        assert_eq!(
            utf16_nul_and_invalid_offsets(&be, 2, false),
            (vec![2], vec![4])
        );
    }

    /// A high surrogate followed by one stray byte (a file cut off mid-way
    /// through a pair): the surrogate is unpaired, and the byte is invalid
    /// on its own. Looking for a low surrogate must not read past the end.
    #[test]
    fn utf16_high_surrogate_then_a_stray_byte() {
        assert_eq!(
            utf16_nul_and_invalid_offsets(&[0xFF, 0xFE, 0x00, 0xD8, 0x41], 2, true),
            (vec![], vec![2, 4])
        );
        assert_eq!(
            utf16_nul_and_invalid_offsets(&[0xFE, 0xFF, 0xD8, 0x00, 0xDC], 2, false),
            (vec![], vec![2, 4])
        );
    }

    #[test]
    fn from_locations_truncates_but_keeps_the_count() {
        let locations = (0..1500).map(|i| Location { row: i, offset: i }).collect();
        let d = Diagnostic::from_locations(DiagnosticKind::NulBytes, locations).unwrap();
        assert_eq!(d.count, 1500);
        assert_eq!(d.first.len(), MAX_LOCATIONS);
        assert!(Diagnostic::from_locations(DiagnosticKind::NulBytes, vec![]).is_none());
    }

    #[test]
    fn severities_match_the_design_table() {
        use DiagnosticKind as K;
        let errors: Vec<_> = K::ALL
            .into_iter()
            .filter(|k| k.severity() == Severity::Error)
            .collect();
        assert_eq!(errors, vec![K::UnterminatedQuote]);
        assert_eq!(K::BomPresent.severity(), Severity::Info);
        assert_eq!(K::NulBytes.severity(), Severity::Warning);
        // ALL is sorted, so `derive` output order matches it.
        let mut sorted = K::ALL;
        sorted.sort();
        assert_eq!(sorted, K::ALL);
    }
}
