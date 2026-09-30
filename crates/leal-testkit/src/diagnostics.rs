//! Diagnostics (DESIGN §3.5): what counts as one occurrence, and where it is.
//!
//! [`derive`] is the testkit's definition of the expected diagnostics for a
//! [`Layout`]. The hand-written corpus sidecars are checked against it (via the
//! reference parser in this crate's tests), and later tasks can compare the
//! real diagnostics against it for generated files.

use serde::Deserialize;

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
    /// Warning. One occurrence per invalid sequence, counted as Rust's
    /// `String::from_utf8_lossy` counts replacement characters (the WHATWG
    /// "maximal subpart" rule). Only for UTF-8 files. Offset: the first byte
    /// of the sequence.
    InvalidEncoding,
    /// Warning. One occurrence per NUL (`0x00`) byte. Offset: the byte.
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

/// The diagnostics a parser should report for `bytes`, given their layout.
/// `utf8` says whether to look for invalid UTF-8 (true for UTF-8 files).
/// The result is sorted by kind, in [`DiagnosticKind::ALL`] order.
#[must_use]
pub fn derive(layout: &Layout, bytes: &[u8], utf8: bool) -> Vec<Diagnostic> {
    let row_of = |offset: usize| layout.row_of_offset(offset).unwrap_or(0);
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

    if utf8 {
        push(
            DiagnosticKind::InvalidEncoding,
            invalid_utf8_offsets(bytes)
                .into_iter()
                .map(|offset| Location {
                    row: row_of(offset),
                    offset,
                })
                .collect(),
        );
    }

    push(
        DiagnosticKind::NulBytes,
        positions_of(0, bytes)
            .map(|offset| Location {
                row: row_of(offset),
                offset,
            })
            .collect(),
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
