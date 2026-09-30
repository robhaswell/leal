//! # TEST ORACLE: not product code
//!
//! A deliberately naive, byte-at-a-time reference parser for the lenient
//! rules in DESIGN §3.4. It exists only to check the testkit against itself:
//! the model generator's layouts, and the hand-written corpus sidecars. It
//! is slow, has no index and no cache, and must never be used by
//! `leal-core`. The real parser (tasks 1.3 and 1.4) is tested *against* the
//! generator and the corpus, never against this file.
//!
//! The rules, as this oracle reads them:
//!
//! 1. A UTF-8 BOM at offset 0 is skipped and belongs to no row.
//! 2. A field is quoted only if its first byte is `"`. Otherwise it runs to
//!    the next delimiter, CR or LF, and any `"` in it is literal.
//! 3. In a quoted field, `""` is one literal `"`; any other `"` closes the
//!    field. Delimiters, CR and LF inside are literal.
//! 4. Bytes from the closing quote to the next delimiter, CR or LF are part
//!    of the field's raw span, and are flagged as text after the closing
//!    quote. A `"` among them is literal. Such a field's display value is
//!    its raw bytes (ADR-0003 decisions 2 and 3).
//! 5. A quoted field with no closing quote runs to the end of the file.
//! 6. Outside quotes, CRLF, a lone CR and a lone LF each end a row. A line
//!    ending at the very end of the file ends the last row, and does not
//!    start another. An empty file has no rows.

// Each integration-test file compiles this module separately and uses a
// different part of it.
#![allow(dead_code)]

use leal_testkit::diagnostics::{self, Diagnostic, DiagnosticKind, Location};
use leal_testkit::dialect::{Bom, Delimiter, Encoding, LineEnding, UTF8_BOM};
use leal_testkit::layout::{FieldLayout, Layout, RowLayout};
use leal_testkit::strategies::csv::{ModelField, ModelRow};

/// Parses `bytes` per the rules above.
pub fn parse(bytes: &[u8], delimiter: Delimiter) -> Layout {
    let d = delimiter.byte();
    let bom_len = if bytes.starts_with(UTF8_BOM) {
        UTF8_BOM.len()
    } else {
        0
    };
    let mut rows = Vec::new();
    let mut pos = bom_len;
    if pos == bytes.len() {
        return Layout { bom_len, rows };
    }
    'rows: loop {
        let row_start = pos;
        let mut fields = Vec::new();
        loop {
            let field = parse_field(bytes, pos, d);
            pos = field.span.end;
            let unterminated = field.unterminated;
            fields.push(field);
            if unterminated || pos == bytes.len() {
                rows.push(RowLayout {
                    span: row_start..pos,
                    line_ending: None,
                    fields,
                });
                break 'rows;
            }
            if bytes[pos] == d {
                pos += 1;
                continue;
            }
            let le = match (bytes[pos], bytes.get(pos + 1)) {
                (b'\r', Some(b'\n')) => LineEnding::Crlf,
                (b'\r', _) => LineEnding::Cr,
                (b'\n', _) => LineEnding::Lf,
                (other, _) => unreachable!("field ended at byte {other:#x}"),
            };
            rows.push(RowLayout {
                span: row_start..pos,
                line_ending: Some(le),
                fields,
            });
            pos += le.byte_len();
            if pos == bytes.len() {
                break 'rows;
            }
            continue 'rows;
        }
    }
    Layout { bom_len, rows }
}

fn parse_field(bytes: &[u8], start: usize, d: u8) -> FieldLayout {
    let is_end = |b: u8| b == d || b == b'\r' || b == b'\n';
    let mut pos = start;
    let mut value = Vec::new();
    if bytes.get(pos) != Some(&b'"') {
        while pos < bytes.len() && !is_end(bytes[pos]) {
            pos += 1;
        }
        return FieldLayout {
            span: start..pos,
            quoted: false,
            value: bytes[start..pos].to_vec(),
            text_after_quote: None,
            unterminated: false,
        };
    }
    pos += 1; // opening quote
    loop {
        match bytes.get(pos) {
            None => {
                return FieldLayout {
                    span: start..pos,
                    quoted: true,
                    value,
                    text_after_quote: None,
                    unterminated: true,
                };
            }
            Some(b'"') if bytes.get(pos + 1) == Some(&b'"') => {
                value.push(b'"');
                pos += 2;
            }
            Some(b'"') => {
                pos += 1; // closing quote
                break;
            }
            Some(&b) => {
                value.push(b);
                pos += 1;
            }
        }
    }
    let after = pos;
    while pos < bytes.len() && !is_end(bytes[pos]) {
        pos += 1;
    }
    let text_after_quote = (pos > after).then_some(after);
    if text_after_quote.is_some() {
        // ADR-0003 decision 2: such a field displays raw.
        value = bytes[start..pos].to_vec();
    }
    FieldLayout {
        span: start..pos,
        quoted: true,
        value,
        text_after_quote,
        unterminated: false,
    }
}

/// Undoes `""` escaping.
fn unescape(inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while i < inner.len() {
        out.push(inner[i]);
        i += if inner[i] == b'"' { 2 } else { 1 };
    }
    out
}

/// Rebuilds model rows from a parse, to check the generator's round trip.
pub fn to_model_rows(layout: &Layout, bytes: &[u8]) -> Vec<ModelRow> {
    layout
        .rows
        .iter()
        .map(|r| ModelRow {
            line_ending: r.line_ending,
            fields: r
                .fields
                .iter()
                .map(|f| {
                    if f.unterminated {
                        ModelField::Unterminated(f.value.clone())
                    } else if let Some(t) = f.text_after_quote {
                        // Between the opening quote and the closing quote at t - 1.
                        let value = unescape(&bytes[f.span.start + 1..t - 1]);
                        let trailing = bytes[t..f.span.end].to_vec();
                        ModelField::Quoted { value, trailing }
                    } else if f.quoted {
                        ModelField::Quoted {
                            value: f.value.clone(),
                            trailing: Vec::new(),
                        }
                    } else {
                        ModelField::Unquoted(f.value.clone())
                    }
                })
                .collect(),
        })
        .collect()
}

/// A parse of a corpus file in its declared encoding.
pub struct Analysis {
    /// The layout. Offsets are always into the file as stored, including for
    /// UTF-16 (ADR-0003 decision 6). For UTF-16 the values are UTF-8.
    pub layout: Layout,
    /// Diagnostics per `leal_testkit::diagnostics::derive`, with file offsets.
    pub diagnostics: Vec<Diagnostic>,
    /// The encoding used to decode values.
    pub encoding: Encoding,
    /// What was actually parsed: the file itself, or for UTF-16 the file
    /// transcoded to UTF-8 (without its BOM), with that text's own layout.
    parsed: Vec<u8>,
    parsed_layout: Layout,
}

/// Parses a corpus file with the delimiter and encoding its sidecar gives.
/// The oracle does no detection: dialect and encoding detection are task
/// 1.2's job, tested against the same sidecars.
pub fn analyze(bytes: &[u8], delimiter: Delimiter, encoding: Encoding) -> Analysis {
    match encoding {
        Encoding::Utf8 | Encoding::Windows1252 => {
            let layout = parse(bytes, delimiter);
            let diagnostics = diagnostics::derive(&layout, bytes, encoding == Encoding::Utf8);
            Analysis {
                parsed_layout: layout.clone(),
                layout,
                diagnostics,
                encoding,
                parsed: bytes.to_vec(),
            }
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            // Parse the UTF-8 transcoding, then map every offset back to the
            // file. Structural characters are single code units, so every
            // span boundary is a character boundary and maps exactly.
            let bom_len = Bom::detect(bytes).bytes().len();
            let (text, map) = decode_utf16(bytes, bom_len, encoding == Encoding::Utf16Le);
            let parsed = text.into_bytes();
            let parsed_layout = parse(&parsed, delimiter);
            let at = |o: usize| map[o];
            let layout = Layout {
                bom_len,
                rows: parsed_layout
                    .rows
                    .iter()
                    .map(|r| RowLayout {
                        span: at(r.span.start)..at(r.span.end),
                        line_ending: r.line_ending,
                        fields: r
                            .fields
                            .iter()
                            .map(|f| FieldLayout {
                                span: at(f.span.start)..at(f.span.end),
                                text_after_quote: f.text_after_quote.map(at),
                                ..f.clone()
                            })
                            .collect(),
                    })
                    .collect(),
            };
            let mut diagnostics = diagnostics::derive(&parsed_layout, &parsed, false);
            for d in &mut diagnostics {
                for l in &mut d.first {
                    l.offset = at(l.offset);
                }
            }
            if bom_len > 0 {
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::BomPresent,
                    count: 1,
                    first: vec![Location { row: 0, offset: 0 }],
                });
            }
            Analysis {
                layout,
                diagnostics,
                encoding,
                parsed,
                parsed_layout,
            }
        }
    }
}

impl Analysis {
    /// The display value of a field: unescaped, then decoded.
    pub fn display_value(&self, row: usize, field: usize) -> Option<String> {
        let f = self.layout.rows.get(row)?.fields.get(field)?;
        Some(match self.encoding {
            Encoding::Windows1252 => decode_windows_1252(&f.value),
            _ => String::from_utf8_lossy(&f.value).into_owned(),
        })
    }

    /// Whether a field is quoted.
    pub fn quoted(&self, row: usize, field: usize) -> Option<bool> {
        Some(self.layout.rows.get(row)?.fields.get(field)?.quoted)
    }

    /// Checks that the parse tiles the text that was parsed.
    pub fn check_tiles(&self, delimiter: Delimiter) -> Result<(), String> {
        self.parsed_layout.check_tiles(&self.parsed, delimiter)
    }
}

/// Decodes UTF-16 after a `bom_len`-byte BOM. Returns the text and, for each
/// byte offset into its UTF-8 form (plus one past the end), the file offset
/// of the character it belongs to.
fn decode_utf16(bytes: &[u8], bom_len: usize, little_endian: bool) -> (String, Vec<usize>) {
    let units: Vec<u16> = bytes[bom_len..]
        .chunks(2)
        .map(|c| {
            let pair = [c[0], c.get(1).copied().unwrap_or(0)];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    let mut text = String::new();
    let mut map = Vec::new();
    let mut unit = 0;
    for r in char::decode_utf16(units.iter().copied()) {
        let (c, width) = match r {
            Ok(c) => (c, c.len_utf16()),
            Err(_) => (char::REPLACEMENT_CHARACTER, 1),
        };
        let file_offset = bom_len + 2 * unit;
        map.extend(std::iter::repeat_n(file_offset, c.len_utf8()));
        text.push(c);
        unit += width;
    }
    map.push(bytes.len());
    (text, map)
}

/// Windows-1252 as the WHATWG Encoding Standard defines it: 0x80–0x9F map
/// to the table below (the five unassigned bytes map to C1 controls), and
/// every other byte maps to the code point of the same value. So no byte is
/// invalid.
pub fn decode_windows_1252(bytes: &[u8]) -> String {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}',
        '\u{017D}', '\u{008F}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
    ];
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => HIGH[usize::from(b - 0x80)],
            _ => char::from(b),
        })
        .collect()
}
