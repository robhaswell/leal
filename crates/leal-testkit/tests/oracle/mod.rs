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
//!    of the field (its raw span and its value), and are flagged as text
//!    after the closing quote. A `"` among them is literal.
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
        value.push(bytes[pos]);
        pos += 1;
    }
    FieldLayout {
        span: start..pos,
        quoted: true,
        value,
        text_after_quote: (pos > after).then_some(after),
        unterminated: false,
    }
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
                    } else if f.quoted {
                        let trailing = f
                            .text_after_quote
                            .map_or(Vec::new(), |t| bytes[t..f.span.end].to_vec());
                        let value = f.value[..f.value.len() - trailing.len()].to_vec();
                        ModelField::Quoted { value, trailing }
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
    /// The layout. For UTF-16 files its offsets are into `parsed`, not the
    /// file.
    pub layout: Layout,
    /// The bytes that were parsed: the file itself, or for UTF-16 the file
    /// transcoded to UTF-8 without its BOM.
    pub parsed: Vec<u8>,
    /// True for UTF-16: offsets in `layout` and `diagnostics` are not file
    /// offsets, so only counts are comparable.
    pub transcoded: bool,
    /// Diagnostics per `leal_testkit::diagnostics::derive`.
    pub diagnostics: Vec<Diagnostic>,
    /// The encoding used to decode values.
    pub encoding: Encoding,
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
                layout,
                parsed: bytes.to_vec(),
                transcoded: false,
                diagnostics,
                encoding,
            }
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let bom = Bom::detect(bytes);
            let body = &bytes[bom.bytes().len()..];
            let parsed = decode_utf16(body, encoding == Encoding::Utf16Le).into_bytes();
            let layout = parse(&parsed, delimiter);
            let mut diagnostics = diagnostics::derive(&layout, &parsed, false);
            if bom != Bom::None {
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::BomPresent,
                    count: 1,
                    first: vec![Location { row: 0, offset: 0 }],
                });
            }
            Analysis {
                layout,
                parsed,
                transcoded: true,
                diagnostics,
                encoding,
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
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units = bytes.chunks(2).map(|c| {
        let pair = [c[0], c.get(1).copied().unwrap_or(0)];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
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
