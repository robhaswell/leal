//! A model-based generator of CSV-like files.
//!
//! The generator first builds a [`CsvModel`]: a dialect plus rows of fields,
//! each field unquoted, quoted (optionally with text after the closing
//! quote) or unterminated. It then serializes the model to bytes and records
//! the [`Layout`] a parser must find: every row and field span, whether each
//! field is quoted, and its display value. Tests use the layout as the
//! oracle for the real parser.
//!
//! # Serialization rules (DESIGN §3.4, ADR-0003)
//!
//! - Fields are separated by the delimiter byte; rows end with LF, CRLF or a
//!   lone CR. The last row may have no line ending.
//! - An unquoted field never contains the delimiter, CR or LF, and never
//!   starts with `"`. With [`Messiness::stray_quotes`] it may contain `"`
//!   after its first byte, which is literal text (`a"b`).
//! - A quoted field is `"`, the value with each `"` doubled, `"`, then any
//!   text after the closing quote. That text never contains the delimiter,
//!   CR or LF, and never *starts* with `"` (the parser would read `""` as an
//!   escaped quote). A `"` later in it is literal (`"a"b"c",` has the text
//!   `b"c"` after its closing quote). Such a field's display value is its
//!   raw bytes, `"a"b"c"`.
//! - An unterminated field is `"` and the value with each `"` doubled, to
//!   the end of the file. It is only ever the last field of the last row,
//!   and that row has no line ending of its own (any newline is inside it).
//! - A UTF-8 BOM, if any, comes first and belongs to no row. A quote right
//!   after the BOM opens a quoted first field.
//!
//! # Things a model must avoid, because the bytes would mean something else
//!
//! The generator repairs these after generating a model (see `normalize`):
//!
//! - A row that serializes to no bytes and has no line ending would vanish.
//!   Such a row gets a quoted empty field (`""`) instead.
//! - A row ending in CR followed by a blank row ending in LF would read as
//!   one CRLF. The blank row's ending becomes CR.
//! - Without a BOM in the model, a file must not *start* with bytes that look
//!   like one (`EF BB BF`, `FF FE`, `FE FF`). The first field is quoted.
//! - Without [`Messiness::blank_lines`], a one-field row whose field is an
//!   empty unquoted value would be a blank line; it is written as `""`.

use std::fmt;

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::select;
use proptest::strategy::Union;

use crate::diagnostics::{self, Diagnostic};
use crate::dialect::{
    Delimiter, Encoding, LineEnding, UTF8_BOM, UTF16BE_BOM, UTF16LE_BOM, expected_encoding,
};
use crate::layout::{FieldLayout, Layout, RowLayout};
use crate::save::Document;
use crate::strategies::bytes::{INVALID_UTF8, MULTIBYTE_UTF8};

/// Which irregular constructs (DESIGN §3.5) the generator may produce. Each
/// flag only *allows* a construct; a given file may or may not contain it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[allow(clippy::struct_excessive_bools)] // independent switches, by design
pub struct Messiness {
    /// Rows may end with different line endings.
    pub mixed_line_endings: bool,
    /// Rows may have more or fewer fields than the rest.
    pub ragged_rows: bool,
    /// Rows may be blank (no bytes before the line ending).
    pub blank_lines: bool,
    /// Unquoted fields may contain `"` after their first byte.
    pub stray_quotes: bool,
    /// Quoted fields may have text between the closing quote and the next
    /// delimiter (`"a"b`).
    pub text_after_closing_quote: bool,
    /// The last field of the file may be an unterminated quoted field.
    pub unterminated_quote: bool,
    /// Values may contain invalid UTF-8.
    pub invalid_utf8: bool,
    /// Values may contain NUL bytes.
    pub nul_bytes: bool,
}

impl Messiness {
    /// Nothing irregular: the file could have been written by a careful
    /// RFC 4180 writer (with any delimiter, line ending and BOM).
    pub const NONE: Messiness = Messiness {
        mixed_line_endings: false,
        ragged_rows: false,
        blank_lines: false,
        stray_quotes: false,
        text_after_closing_quote: false,
        unterminated_quote: false,
        invalid_utf8: false,
        nul_bytes: false,
    };

    /// Every irregular construct allowed.
    pub const ALL: Messiness = Messiness {
        mixed_line_endings: true,
        ragged_rows: true,
        blank_lines: true,
        stray_quotes: true,
        text_after_closing_quote: true,
        unterminated_quote: true,
        invalid_utf8: true,
        nul_bytes: true,
    };
}

/// Settings for [`csv_file`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CsvConfig {
    /// Which irregular constructs are allowed.
    pub messiness: Messiness,
    /// Most rows in a file (the least is 0).
    pub max_rows: usize,
    /// Most fields in a regular row (the least is 1). Ragged rows may have
    /// up to two more.
    pub max_fields: usize,
    /// Most chunks in one value; each chunk is 1 to 5 bytes.
    pub max_value_chunks: usize,
}

impl CsvConfig {
    /// Small, regular files: no irregular constructs.
    #[must_use]
    pub fn clean() -> Self {
        CsvConfig {
            messiness: Messiness::NONE,
            max_rows: 10,
            max_fields: 5,
            max_value_chunks: 4,
        }
    }

    /// Small files that may contain every irregular construct.
    #[must_use]
    pub fn messy() -> Self {
        CsvConfig {
            messiness: Messiness::ALL,
            ..CsvConfig::clean()
        }
    }
}

impl Default for CsvConfig {
    fn default() -> Self {
        CsvConfig::clean()
    }
}

/// How the model chooses to quote fields that don't need quoting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotingStyle {
    /// Quote only when the value needs it.
    Minimal,
    /// Quote every field.
    Always,
    /// Quote some fields at random, as hand-edited files do.
    Mixed,
}

/// The line endings the model uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEndings {
    /// Every row uses this one.
    Uniform(LineEnding),
    /// Each row picks its own.
    Mixed,
}

/// The dialect a model was generated with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelDialect {
    /// The field delimiter.
    pub delimiter: Delimiter,
    /// The line endings used.
    pub line_endings: LineEndings,
    /// Whether the file starts with a UTF-8 BOM.
    pub bom: bool,
    /// How fields are quoted.
    pub quoting: QuotingStyle,
}

/// One field of the model.
#[derive(Clone, PartialEq, Eq)]
pub enum ModelField {
    /// Written as is.
    Unquoted(Vec<u8>),
    /// Written as `"value"` with quotes doubled, then `trailing` verbatim.
    Quoted {
        /// The value between the quotes, unescaped.
        value: Vec<u8>,
        /// Text after the closing quote (usually empty).
        trailing: Vec<u8>,
    },
    /// Written as `"value` with quotes doubled, running to the end of file.
    Unterminated(Vec<u8>),
}

impl ModelField {
    /// The field's display value, as [`FieldLayout::value`] defines it. A
    /// quoted field with text after its closing quote shows its raw bytes.
    #[must_use]
    pub fn value(&self) -> Vec<u8> {
        match self {
            ModelField::Unquoted(v) | ModelField::Unterminated(v) => v.clone(),
            ModelField::Quoted { value, trailing } if trailing.is_empty() => value.clone(),
            ModelField::Quoted { .. } => self.raw(),
        }
    }

    /// The field's bytes as written to the file.
    #[must_use]
    pub fn raw(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            ModelField::Unquoted(v) => out.extend_from_slice(v),
            ModelField::Quoted { value, trailing } => {
                out.push(b'"');
                push_escaped(&mut out, value);
                out.push(b'"');
                out.extend_from_slice(trailing);
            }
            ModelField::Unterminated(v) => {
                out.push(b'"');
                push_escaped(&mut out, v);
            }
        }
        out
    }
}

impl fmt::Debug for ModelField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelField::Unquoted(v) => write!(f, "Unquoted(\"{}\")", v.escape_ascii()),
            ModelField::Quoted { value, trailing } if trailing.is_empty() => {
                write!(f, "Quoted(\"{}\")", value.escape_ascii())
            }
            ModelField::Quoted { value, trailing } => write!(
                f,
                "Quoted(\"{}\", trailing: \"{}\")",
                value.escape_ascii(),
                trailing.escape_ascii()
            ),
            ModelField::Unterminated(v) => write!(f, "Unterminated(\"{}\")", v.escape_ascii()),
        }
    }
}

/// One row of the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    /// At least one field.
    pub fields: Vec<ModelField>,
    /// The row's line ending; `None` only for the last row.
    pub line_ending: Option<LineEnding>,
}

impl ModelRow {
    /// True if the row is a single empty unquoted field: it writes no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.fields.as_slice(), [ModelField::Unquoted(v)] if v.is_empty())
    }
}

/// A generated file, before serialization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvModel {
    /// How the file is written.
    pub dialect: ModelDialect,
    /// The rows.
    pub rows: Vec<ModelRow>,
}

impl CsvModel {
    /// Writes the model out, returning the bytes and their layout.
    #[must_use]
    pub fn serialize(&self) -> (Vec<u8>, Layout) {
        let delimiter = self.dialect.delimiter.byte();
        let mut out = Vec::new();
        if self.dialect.bom {
            out.extend_from_slice(UTF8_BOM);
        }
        let bom_len = out.len();
        let mut rows = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let row_start = out.len();
            let mut fields = Vec::with_capacity(row.fields.len());
            for (i, field) in row.fields.iter().enumerate() {
                if i > 0 {
                    out.push(delimiter);
                }
                let start = out.len();
                out.extend_from_slice(&field.raw());
                let (quoted, text_after_quote, unterminated) = match field {
                    ModelField::Unquoted(_) => (false, None, false),
                    ModelField::Quoted { trailing, .. } => {
                        let after = out.len() - trailing.len();
                        (true, (!trailing.is_empty()).then_some(after), false)
                    }
                    ModelField::Unterminated(_) => (true, None, true),
                };
                fields.push(FieldLayout {
                    span: start..out.len(),
                    quoted,
                    value: field.value(),
                    text_after_quote,
                    unterminated,
                });
            }
            let row_end = out.len();
            if let Some(le) = row.line_ending {
                out.extend_from_slice(le.bytes());
            }
            rows.push(RowLayout {
                span: row_start..row_end,
                line_ending: row.line_ending,
                fields,
            });
        }
        (out, Layout { bom_len, rows })
    }

    /// True if the last row has a line ending.
    #[must_use]
    pub fn trailing_newline(&self) -> bool {
        self.rows.last().is_some_and(|r| r.line_ending.is_some())
    }

    /// Checks every rule in this module's documentation, returning the first
    /// one broken. Every model [`csv_file`] generates passes.
    ///
    /// # Errors
    ///
    /// Returns a description of the first broken rule.
    pub fn check(&self) -> Result<(), String> {
        let d = self.dialect.delimiter.byte();
        let structural = |b: &u8| *b == d || *b == b'\r' || *b == b'\n';
        let last = self.rows.len().saturating_sub(1);
        for (ri, row) in self.rows.iter().enumerate() {
            if row.fields.is_empty() {
                return Err(format!("row {ri} has no fields"));
            }
            if row.line_ending.is_none() && ri != last {
                return Err(format!("row {ri} has no line ending but is not last"));
            }
            if row.line_ending.is_none() && row.is_empty() {
                return Err(format!("row {ri} is empty and has no line ending"));
            }
            if ri > 0
                && self.rows[ri - 1].line_ending == Some(LineEnding::Cr)
                && row.is_empty()
                && row.line_ending == Some(LineEnding::Lf)
            {
                return Err(format!("row {ri} is a blank LF line after a CR"));
            }
            for (fi, field) in row.fields.iter().enumerate() {
                match field {
                    ModelField::Unquoted(v) => {
                        if v.iter().any(structural) {
                            return Err(format!("unquoted {ri}:{fi} has a delimiter, CR or LF"));
                        }
                        if v.first() == Some(&b'"') {
                            return Err(format!("unquoted {ri}:{fi} starts with a quote"));
                        }
                    }
                    ModelField::Quoted { trailing, .. } => {
                        if trailing.iter().any(structural) {
                            return Err(format!(
                                "trailing text {ri}:{fi} has a delimiter, CR or LF"
                            ));
                        }
                        if trailing.first() == Some(&b'"') {
                            return Err(format!("trailing text {ri}:{fi} starts with a quote"));
                        }
                    }
                    ModelField::Unterminated(_) => {
                        if ri != last || fi + 1 != row.fields.len() || row.line_ending.is_some() {
                            return Err(format!(
                                "unterminated {ri}:{fi} is not the end of the file"
                            ));
                        }
                    }
                }
            }
        }
        if !self.dialect.bom {
            let (bytes, _) = self.serialize();
            if starts_with_bom(&bytes) {
                return Err("the file starts with BOM bytes but the model has no BOM".to_owned());
            }
        }
        Ok(())
    }
}

/// A generated file: the model, its bytes, and what a parser must find.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedCsv {
    /// The model the bytes were written from.
    pub model: CsvModel,
    /// The file's bytes.
    pub bytes: Vec<u8>,
    /// Every row and field, with spans and values.
    pub layout: Layout,
    /// The encoding ADR-0003 gives these bytes ([`expected_encoding`]):
    /// UTF-8, or Windows-1252 if invalid bytes outnumber valid multibyte
    /// sequences.
    pub encoding: Encoding,
    /// The diagnostics a parser should report, from [`diagnostics::derive`].
    /// `invalid_encoding` appears only if `encoding` is UTF-8.
    pub diagnostics: Vec<Diagnostic>,
}

impl GeneratedCsv {
    /// Serializes `model` and derives the expected layout, encoding and
    /// diagnostics.
    #[must_use]
    pub fn from_model(model: CsvModel) -> Self {
        let (bytes, layout) = model.serialize();
        let encoding = expected_encoding(&bytes);
        let diagnostics = diagnostics::derive(&layout, &bytes, encoding);
        GeneratedCsv {
            model,
            bytes,
            layout,
            encoding,
            diagnostics,
        }
    }

    /// The delimiter the file was written with.
    #[must_use]
    pub fn delimiter(&self) -> Delimiter {
        self.model.dialect.delimiter
    }

    /// A [`Document`] over this file with no edits, for the save oracle.
    #[must_use]
    pub fn document(&self) -> Document<'_> {
        Document::new(&self.bytes, &self.layout, self.delimiter(), self.encoding)
    }

    /// The same model written as UTF-16 with a BOM. Spans and diagnostic
    /// locations are byte offsets into the UTF-16 file (ADR-0003 decision
    /// 6); field values stay UTF-8, as [`FieldLayout::value`] says. A UTF-8
    /// BOM in the model is dropped, since the UTF-16 BOM replaces it.
    ///
    /// # Panics
    ///
    /// Panics if a value is not valid UTF-8 (generate with
    /// `Messiness::invalid_utf8` off, as [`csv_file_utf16`] does).
    #[must_use]
    pub fn into_utf16(self, little_endian: bool) -> GeneratedCsv {
        self.into_utf16_with(little_endian, &Utf16Faults::default())
    }

    /// [`into_utf16`](Self::into_utf16), with text that isn't valid UTF-16
    /// (ADR-0003 decision 7, ADR-0008 decision 7):
    ///
    /// - each of `faults.lone_surrogates` picks a character of a value (any
    ///   but the delimiter, `"`, CR and LF) and writes it as an **unpaired
    ///   surrogate**: a high one, or a low one where a high one would be
    ///   followed by the next pick's low one and pair with it. The model and
    ///   the layout hold U+FFFD there, which is how it reads;
    /// - `faults.odd_byte` adds that byte at the end of the file, **a final
    ///   odd byte**, which isn't a whole code unit. It belongs to the last
    ///   row (a new one, after a line ending), and reads as U+FFFD. It is in
    ///   the bytes and the layout only, not in the model.
    ///
    /// # Panics
    ///
    /// As [`into_utf16`](Self::into_utf16).
    #[must_use]
    pub fn into_utf16_with(self, little_endian: bool, faults: &Utf16Faults) -> GeneratedCsv {
        let mut model = self.model;
        model.dialect.bom = false;
        let delimiter = model.dialect.delimiter.byte();
        for pick in &faults.lone_surrogates {
            replace_with_lone_surrogate(&mut model.rows, delimiter, *pick);
        }
        normalize(&mut model.rows, false, Messiness::ALL);
        let (utf8, layout8) = model.serialize();
        let text = std::str::from_utf8(&utf8).expect("UTF-16 models need valid UTF-8 values");
        let (bom, encoding) = if little_endian {
            (UTF16LE_BOM, Encoding::Utf16Le)
        } else {
            (UTF16BE_BOM, Encoding::Utf16Be)
        };
        let mut bytes = bom.to_vec();
        let mut map = vec![0; utf8.len() + 1];
        let mut units = [0u16; 2];
        let mut chars = text.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            map[i..i + c.len_utf8()].fill(bytes.len());
            let lone;
            let written: &[u16] = if c == char::REPLACEMENT_CHARACTER {
                // No value holds U+FFFD but a fault's, which is an unpaired
                // surrogate. A high one is unpaired unless a low one comes
                // next, which only another fault's can be.
                let next_is_fault =
                    chars.peek().map(|&(_, n)| n) == Some(char::REPLACEMENT_CHARACTER);
                lone = [if next_is_fault { LONE_LOW } else { LONE_HIGH }];
                &lone
            } else {
                c.encode_utf16(&mut units)
            };
            for &unit in written {
                let pair = if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
        }
        map[utf8.len()] = bytes.len();
        let at = |o: usize| map[o];
        let mut layout = Layout {
            bom_len: bom.len(),
            rows: layout8
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
        if let Some(odd) = faults.odd_byte {
            add_odd_byte(&mut layout, &model, bytes.len());
            bytes.push(odd);
        }
        let diagnostics = diagnostics::derive(&layout, &bytes, encoding);
        GeneratedCsv {
            model,
            bytes,
            layout,
            encoding,
            diagnostics,
        }
    }

    /// The same model written without a UTF-8 BOM (repaired as a model
    /// without one must be: a first field that would start with BOM-like
    /// bytes is quoted). Unchanged if it had none.
    #[must_use]
    pub fn without_bom(self) -> GeneratedCsv {
        if !self.model.dialect.bom {
            return self;
        }
        let mut model = self.model;
        model.dialect.bom = false;
        normalize(&mut model.rows, false, Messiness::ALL);
        GeneratedCsv::from_model(model)
    }

    /// The same file, opened as `encoding`, a single-byte encoding: the
    /// bytes and their layout as they are (every structural byte is ASCII
    /// in all of them), the values read in `encoding`
    /// ([`crate::dialect::decode_value`]), the diagnostics derived for it.
    /// Bytes `encoding` leaves unassigned stay, and read as U+FFFD
    /// (`invalid_encoding`). The other single-byte encodings come only from
    /// the file's attribute or the user's choice (ADR-0005 decision 5).
    ///
    /// # Panics
    ///
    /// If `encoding` isn't single-byte, or the file has a BOM, which only
    /// its own encoding can read.
    #[must_use]
    pub fn in_single_byte(self, encoding: Encoding) -> GeneratedCsv {
        assert!(encoding.is_single_byte(), "{encoding:?}");
        assert_eq!(
            self.layout.bom_len, 0,
            "a file with a BOM is read in its own encoding"
        );
        let diagnostics = diagnostics::derive(&self.layout, &self.bytes, encoding);
        GeneratedCsv {
            encoding,
            diagnostics,
            ..self
        }
    }
}

/// What [`GeneratedCsv::into_utf16_with`] writes that isn't valid UTF-16.
#[derive(Clone, Debug, Default)]
pub struct Utf16Faults {
    /// Characters of values written as unpaired surrogates: each picks the
    /// character at that place (modulo their number) among all those that
    /// can be, in file order.
    pub lone_surrogates: Vec<usize>,
    /// A final odd byte.
    pub odd_byte: Option<u8>,
}

/// The high surrogate a fault writes when it can't pair (U+D83D, the first
/// unit of most emoji).
const LONE_HIGH: u16 = 0xD83D;
/// The low surrogate a fault writes before another fault (U+DE00).
const LONE_LOW: u16 = 0xDC00;

/// The odd bytes a UTF-16 file may end with: a NUL, an LF, a quote and a
/// letter, which would be structural or text in a whole unit, and a byte
/// of a surrogate.
const ODD_BYTES: [u8; 5] = [0x00, b'\n', b'"', b'A', 0xD8];

/// Replaces the character `pick` chooses, among those of every value (and
/// text after a closing quote) that aren't the delimiter, `"`, CR or LF,
/// with U+FFFD. Nothing if there is none.
fn replace_with_lone_surrogate(rows: &mut [ModelRow], delimiter: u8, pick: usize) {
    let structural = |c: char| {
        u8::try_from(u32::from(c))
            .is_ok_and(|b| b == delimiter || b == b'"' || b == b'\r' || b == b'\n')
    };
    let mut parts: Vec<&mut Vec<u8>> = rows
        .iter_mut()
        .flat_map(|r| r.fields.iter_mut())
        .flat_map(|f| match f {
            ModelField::Unquoted(v) | ModelField::Unterminated(v) => vec![v],
            ModelField::Quoted { value, trailing } => vec![value, trailing],
        })
        .collect();
    let eligible = |part: &Vec<u8>| {
        String::from_utf8_lossy(part)
            .chars()
            .filter(|&c| !structural(c))
            .count()
    };
    let total: usize = parts.iter().map(|p| eligible(p)).sum();
    if total == 0 {
        return;
    }
    let mut n = pick % total;
    for part in &mut parts {
        let here = eligible(part);
        if n >= here {
            n -= here;
            continue;
        }
        let text = String::from_utf8_lossy(part).into_owned();
        let mut seen = 0;
        let replaced: String = text
            .chars()
            .map(|c| {
                if structural(c) {
                    return c;
                }
                seen += 1;
                if seen == n + 1 {
                    char::REPLACEMENT_CHARACTER
                } else {
                    c
                }
            })
            .collect();
        **part = replaced.into_bytes();
        return;
    }
}

/// Adds a final odd byte at `at` (the end of the file) to `layout`, as a
/// parser finds it: a row of its own after a line ending (or in a file with
/// no rows), otherwise the end of the last field, read as U+FFFD. After a
/// closing quote, that is text after it, so the field shows its raw text.
fn add_odd_byte(layout: &mut Layout, model: &CsvModel, at: usize) {
    let replacement = char::REPLACEMENT_CHARACTER.to_string().into_bytes();
    let needs_row = layout.rows.last().is_none_or(|r| r.line_ending.is_some());
    if needs_row {
        layout.rows.push(RowLayout {
            span: at..at + 1,
            line_ending: None,
            fields: vec![FieldLayout {
                span: at..at + 1,
                quoted: false,
                value: replacement,
                text_after_quote: None,
                unterminated: false,
            }],
        });
        return;
    }
    let (Some(row), Some(model_field)) = (
        layout.rows.last_mut(),
        model.rows.last().and_then(|r| r.fields.last()),
    ) else {
        return;
    };
    row.span.end += 1;
    let Some(field) = row.fields.last_mut() else {
        return;
    };
    field.span.end += 1;
    match model_field {
        ModelField::Quoted { trailing, .. } if trailing.is_empty() => {
            // A closed quote: the byte is text after it, and the field
            // shows its raw text.
            field.text_after_quote = Some(at);
            field.value = model_field.raw();
            field.value.extend_from_slice(&replacement);
        }
        _ => field.value.extend_from_slice(&replacement),
    }
}

/// Like [`csv_file`], but written as UTF-16 LE or BE with a BOM; see
/// [`GeneratedCsv::into_utf16`]. NUL values become U+0000 code units.
/// `Messiness::invalid_utf8` means text that isn't valid UTF-16 instead
/// ([`GeneratedCsv::into_utf16_with`]): in a third of the files, one or
/// two unpaired surrogates in values; in one in five, a final odd byte.
pub fn csv_file_utf16(config: CsvConfig) -> impl Strategy<Value = GeneratedCsv> {
    let invalid = config.messiness.invalid_utf8;
    let mut config = config;
    config.messiness.invalid_utf8 = false;
    let faults = if invalid {
        (
            prop_oneof![2 => Just(Vec::new()), 1 => vec(any::<usize>(), 1..=2)],
            prop::option::weighted(0.2, select(&ODD_BYTES[..])),
        )
            .prop_map(|(lone_surrogates, odd_byte)| Utf16Faults {
                lone_surrogates,
                odd_byte,
            })
            .boxed()
    } else {
        Just(Utf16Faults::default()).boxed()
    };
    (csv_file(config), any::<bool>(), faults)
        .prop_map(|(file, le, faults)| file.into_utf16_with(le, &faults))
}

impl fmt::Debug for GeneratedCsv {
    // Proptest prints this for a failing case, so keep it readable: the bytes
    // as an escaped string, then the model. The layout is derivable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<_> = self.diagnostics.iter().map(|d| (d.kind, d.count)).collect();
        f.debug_struct("GeneratedCsv")
            .field("bytes", &format_args!("b\"{}\"", self.bytes.escape_ascii()))
            .field("dialect", &self.model.dialect)
            .field("encoding", &self.encoding)
            .field("rows", &self.model.rows)
            .field("diagnostics", &kinds)
            .finish()
    }
}

/// A strategy for generated CSV-like files. See the module documentation.
///
/// ```
/// use leal_testkit::strategies::csv::{CsvConfig, csv_file};
/// use proptest::prelude::*;
///
/// proptest! {
///     // In a test file, put `#[test]` on the line above `fn`.
///     fn spans_tile_the_file(file in csv_file(CsvConfig::messy())) {
///         prop_assert_eq!(file.layout.check_tiles(&file.bytes, file.delimiter()), Ok(()));
///     }
/// }
/// # spans_tile_the_file();
/// ```
pub fn csv_file(config: CsvConfig) -> impl Strategy<Value = GeneratedCsv> {
    let m = config.messiness;
    let file_level = (
        select(&Delimiter::ALL[..]),
        line_endings(m),
        prop::bool::weighted(0.25), // BOM
        select(
            &[
                QuotingStyle::Minimal,
                QuotingStyle::Always,
                QuotingStyle::Mixed,
            ][..],
        ),
        prop::bool::weighted(0.75), // trailing newline
        prop::bool::weighted(0.3),  // unterminated, if allowed
        1..=config.max_fields.max(1),
    );
    file_level.prop_flat_map(
        move |(delimiter, line_endings, bom, quoting, trailing, unterminated, n)| {
            let dialect = ModelDialect {
                delimiter,
                line_endings,
                bom,
                quoting,
            };
            vec(raw_row(n, config), 0..=config.max_rows).prop_map(move |rows| {
                let raw = RawFile {
                    dialect,
                    trailing_newline: trailing,
                    unterminated,
                    fields: n,
                    rows,
                };
                GeneratedCsv::from_model(build(raw, m))
            })
        },
    )
}

// ---- generation ------------------------------------------------------------

/// The random choices behind one file, before the rules are applied.
struct RawFile {
    dialect: ModelDialect,
    trailing_newline: bool,
    unterminated: bool,
    /// Fields in a regular row.
    fields: usize,
    rows: Vec<RawRow>,
}

#[derive(Clone, Debug)]
struct RawRow {
    /// `fields + 2` candidates, so that ragged rows can be longer.
    fields: Vec<RawField>,
    /// Change in field count, for ragged rows.
    delta: i8,
    blank: bool,
    line_ending: LineEnding,
}

#[derive(Clone, Debug)]
struct RawField {
    value: Vec<u8>,
    /// Quote this field if the quoting style is `Mixed`.
    quote: bool,
    /// Text after the closing quote, if the field ends up quoted.
    trailing: Option<Vec<u8>>,
}

fn line_endings(m: Messiness) -> BoxedStrategy<LineEndings> {
    let uniform = select(&LineEnding::ALL[..]).prop_map(LineEndings::Uniform);
    if m.mixed_line_endings {
        prop_oneof![2 => uniform, 1 => Just(LineEndings::Mixed)].boxed()
    } else {
        uniform.boxed()
    }
}

fn raw_row(fields: usize, config: CsvConfig) -> impl Strategy<Value = RawRow> {
    (
        vec(raw_field(config), fields + 2),
        prop_oneof![5 => Just(0i8), 1 => -2i8..=2],
        prop::bool::weighted(0.12),
        select(&LineEnding::ALL[..]),
    )
        .prop_map(|(fields, delta, blank, line_ending)| RawRow {
            fields,
            delta,
            blank,
            line_ending,
        })
}

fn raw_field(config: CsvConfig) -> impl Strategy<Value = RawField> {
    let trailing = vec(select(&b"xyz \"';,\t|"[..]), 1..=3);
    (
        value(config),
        any::<bool>(),
        prop::option::weighted(0.15, trailing),
    )
        .prop_map(|(value, quote, trailing)| RawField {
            value,
            quote,
            trailing,
        })
}

/// A field value built from chunks: words, CSV-significant bytes, multibyte
/// UTF-8, and (if allowed) invalid UTF-8 and NUL.
fn value(config: CsvConfig) -> impl Strategy<Value = Vec<u8>> {
    let words = vec(
        select(&b"abcdefghijklmnopqrstuvwxyz0123456789 .-"[..]),
        1..=5,
    );
    let mut chunks: Vec<(u32, BoxedStrategy<Vec<u8>>)> = vec![
        (6, words.boxed()),
        (3, select(&SPECIALS[..]).prop_map(<[u8]>::to_vec).boxed()),
        (
            2,
            select(&MULTIBYTE_UTF8[..])
                .prop_map(|s| s.as_bytes().to_vec())
                .boxed(),
        ),
    ];
    if config.messiness.invalid_utf8 {
        chunks.push((
            1,
            select(&INVALID_UTF8[..]).prop_map(<[u8]>::to_vec).boxed(),
        ));
    }
    if config.messiness.nul_bytes {
        chunks.push((1, Just(vec![0u8]).boxed()));
    }
    vec(Union::new_weighted(chunks), 0..=config.max_value_chunks).prop_map(|c| c.concat())
}

/// Value chunks that force quoting or test quoting.
const SPECIALS: [&[u8]; 8] = [b",", b";", b"\t", b"|", b"\"", b"\r", b"\n", b"\r\n"];

/// Turns random choices into a model that obeys the module's rules.
fn build(raw: RawFile, m: Messiness) -> CsvModel {
    let delimiter = raw.dialect.delimiter.byte();
    let unterminated = m.unterminated_quote && raw.unterminated;
    let last = raw.rows.len().saturating_sub(1);
    let last_has_ending = raw.trailing_newline && !unterminated;
    let mut rows: Vec<ModelRow> = raw
        .rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            // A blank last row with no line ending would be no bytes at all,
            // so the last row is only blank if it has a line ending.
            let blank = m.blank_lines && r.blank && (i != last || last_has_ending);
            let fields = if blank {
                vec![ModelField::Unquoted(Vec::new())]
            } else {
                let count = if m.ragged_rows {
                    let n = i64::try_from(raw.fields).unwrap_or(i64::MAX) + i64::from(r.delta);
                    usize::try_from(n.max(1)).unwrap_or(1)
                } else {
                    raw.fields
                };
                r.fields
                    .into_iter()
                    .take(count)
                    .map(|f| build_field(f, delimiter, raw.dialect.quoting, m))
                    .collect()
            };
            let line_ending = match raw.dialect.line_endings {
                LineEndings::Uniform(le) => le,
                LineEndings::Mixed => r.line_ending,
            };
            ModelRow {
                fields,
                line_ending: Some(line_ending),
            }
        })
        .collect();

    if let Some(last) = rows.last_mut() {
        if unterminated {
            if let Some(field) = last.fields.last_mut() {
                *field = ModelField::Unterminated(field.value());
            }
            last.line_ending = None;
        } else if !raw.trailing_newline {
            last.line_ending = None;
        }
    }

    normalize(&mut rows, raw.dialect.bom, m);
    CsvModel {
        dialect: raw.dialect,
        rows,
    }
}

fn build_field(f: RawField, delimiter: u8, quoting: QuotingStyle, m: Messiness) -> ModelField {
    let value = f.value;
    let needs_quotes = value.first() == Some(&b'"')
        || value
            .iter()
            .any(|&b| b == delimiter || b == b'\r' || b == b'\n')
        || (!m.stray_quotes && value.contains(&b'"'));
    let quoted = needs_quotes
        || match quoting {
            QuotingStyle::Minimal => false,
            QuotingStyle::Always => true,
            QuotingStyle::Mixed => f.quote,
        };
    if !quoted {
        return ModelField::Unquoted(value);
    }
    let mut trailing: Vec<u8> = if m.text_after_closing_quote {
        f.trailing
            .unwrap_or_default()
            .into_iter()
            .filter(|&b| b != delimiter && b != b'\r' && b != b'\n')
            .collect()
    } else {
        Vec::new()
    };
    let leading_quotes = trailing.iter().take_while(|&&b| b == b'"').count();
    trailing.drain(..leading_quotes);
    ModelField::Quoted { value, trailing }
}

/// Repairs the cases where the bytes would not mean what the model says.
fn normalize(rows: &mut [ModelRow], bom: bool, m: Messiness) {
    let quoted_empty = || ModelField::Quoted {
        value: Vec::new(),
        trailing: Vec::new(),
    };
    for row in rows.iter_mut() {
        if row.is_empty() && (!m.blank_lines || row.line_ending.is_none()) {
            row.fields[0] = quoted_empty();
        }
    }
    for i in 1..rows.len() {
        if rows[i - 1].line_ending == Some(LineEnding::Cr)
            && rows[i].is_empty()
            && rows[i].line_ending == Some(LineEnding::Lf)
        {
            rows[i].line_ending = Some(LineEnding::Cr);
        }
    }
    // `if let ... && ...` chains several conditions; each `let` must match.
    if !bom
        && let Some(first) = rows.first_mut().and_then(|r| r.fields.first_mut())
        && let ModelField::Unquoted(v) = first
        && starts_with_bom(v)
    {
        *first = ModelField::Quoted {
            value: std::mem::take(v),
            trailing: Vec::new(),
        };
    }
}

fn starts_with_bom(bytes: &[u8]) -> bool {
    [UTF8_BOM, UTF16LE_BOM, UTF16BE_BOM]
        .iter()
        .any(|bom| bytes.starts_with(bom))
}

fn push_escaped(out: &mut Vec<u8>, value: &[u8]) {
    for &b in value {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialect(bom: bool) -> ModelDialect {
        ModelDialect {
            delimiter: Delimiter::Comma,
            line_endings: LineEndings::Uniform(LineEnding::Lf),
            bom,
            quoting: QuotingStyle::Minimal,
        }
    }

    fn row(fields: Vec<ModelField>, le: Option<LineEnding>) -> ModelRow {
        ModelRow {
            fields,
            line_ending: le,
        }
    }

    fn unq(s: &str) -> ModelField {
        ModelField::Unquoted(s.as_bytes().to_vec())
    }

    #[test]
    fn serializes_every_field_kind() {
        let model = CsvModel {
            dialect: dialect(true),
            rows: vec![
                row(
                    vec![
                        unq("a\"b"),
                        ModelField::Quoted {
                            value: b"x,\"y\"\n".to_vec(),
                            trailing: Vec::new(),
                        },
                        ModelField::Quoted {
                            value: b"q".to_vec(),
                            trailing: b"t\"u".to_vec(),
                        },
                    ],
                    Some(LineEnding::Crlf),
                ),
                row(vec![unq("")], Some(LineEnding::Lf)),
                row(
                    vec![
                        unq("1"),
                        ModelField::Unterminated(b"rest \"of\"\nfile".to_vec()),
                    ],
                    None,
                ),
            ],
        };
        assert_eq!(model.check(), Ok(()));
        let (bytes, layout) = model.serialize();
        let expected: &[u8] =
            b"\xEF\xBB\xBFa\"b,\"x,\"\"y\"\"\n\",\"q\"t\"u\r\n\n1,\"rest \"\"of\"\"\nfile";
        assert_eq!(
            bytes.escape_ascii().to_string(),
            expected.escape_ascii().to_string()
        );
        assert_eq!(layout.bom_len, 3);
        assert_eq!(layout.rows.len(), 3);
        let f = &layout.rows[0].fields;
        assert_eq!(f[0].span, 3..6);
        assert_eq!(f[1].span, 7..17);
        assert_eq!(f[1].value, b"x,\"y\"\n");
        assert_eq!(
            f[2].value, b"\"q\"t\"u",
            "text after a closing quote shows raw"
        );
        assert_eq!(f[2].span, 18..24);
        assert_eq!(f[2].text_after_quote, Some(21));
        assert!(layout.rows[1].is_blank());
        assert!(layout.rows[2].fields[1].unterminated);
        assert!(!layout.trailing_newline());
        assert_eq!(layout.check_tiles(&bytes, Delimiter::Comma), Ok(()));
    }

    #[test]
    fn check_rejects_broken_models() {
        let bad = |rows| {
            CsvModel {
                dialect: dialect(false),
                rows,
            }
            .check()
        };
        assert!(bad(vec![row(vec![unq("a,b")], None)]).is_err());
        assert!(bad(vec![row(vec![unq("\"a")], None)]).is_err());
        assert!(bad(vec![row(vec![unq("")], None)]).is_err());
        assert!(bad(vec![row(vec![unq("a")], None), row(vec![unq("b")], None)]).is_err());
        assert!(
            bad(vec![row(
                vec![ModelField::Quoted {
                    value: vec![],
                    trailing: b"\"x".to_vec()
                }],
                None
            )])
            .is_err()
        );
        assert!(
            bad(vec![
                row(vec![ModelField::Unterminated(vec![])], Some(LineEnding::Lf)),
                row(vec![unq("b")], None)
            ])
            .is_err()
        );
        // CR or LF in an unquoted value or in text after a closing quote.
        assert!(bad(vec![row(vec![unq("a\rb")], None)]).is_err());
        assert!(bad(vec![row(vec![unq("a\nb")], None)]).is_err());
        for trailing in [b"x\r", b"x\n"] {
            assert!(
                bad(vec![row(
                    vec![ModelField::Quoted {
                        value: vec![],
                        trailing: trailing.to_vec()
                    }],
                    None
                )])
                .is_err()
            );
        }
        // An unterminated field that isn't the file's last field, or that is
        // followed by a line ending.
        assert!(
            bad(vec![row(
                vec![ModelField::Unterminated(b"a".to_vec()), unq("b")],
                None
            )])
            .is_err()
        );
        assert!(
            bad(vec![row(
                vec![ModelField::Unterminated(b"a".to_vec())],
                Some(LineEnding::Lf)
            )])
            .is_err()
        );
        assert!(bad(vec![row(vec![unq("\u{FEFF}a")], None)]).is_err());
        assert!(
            bad(vec![
                row(vec![unq("a")], Some(LineEnding::Cr)),
                row(vec![unq("")], Some(LineEnding::Lf)),
            ])
            .is_err()
        );
    }

    /// Proptest prints models with this, so every kind of field must show.
    #[test]
    fn model_field_debug_shows_escaped_bytes() {
        let printed = format!(
            "{:?}",
            [
                unq("a\r"),
                ModelField::Quoted {
                    value: b"x\"".to_vec(),
                    trailing: vec![],
                },
                ModelField::Quoted {
                    value: b"q".to_vec(),
                    trailing: b"t".to_vec(),
                },
                ModelField::Unterminated(b"\xFF".to_vec()),
            ]
        );
        assert_eq!(
            printed,
            r#"[Unquoted("a\r"), Quoted("x\""), Quoted("q", trailing: "t"), Unterminated("\xff")]"#
        );
    }

    #[test]
    fn normalize_repairs_ambiguous_models() {
        let mut rows = vec![
            row(vec![unq("\u{FEFF}x")], Some(LineEnding::Cr)),
            row(vec![unq("")], Some(LineEnding::Lf)),
            row(vec![unq("")], None),
        ];
        normalize(&mut rows, false, Messiness::ALL);
        assert!(matches!(rows[0].fields[0], ModelField::Quoted { .. }));
        assert_eq!(rows[1].line_ending, Some(LineEnding::Cr));
        assert!(
            rows[1].is_empty(),
            "blank lines are allowed, so it stays blank"
        );
        assert!(!rows[2].is_empty(), "an empty last row would vanish");

        let mut rows = vec![row(vec![unq("")], Some(LineEnding::Lf))];
        normalize(&mut rows, false, Messiness::NONE);
        assert!(!rows[0].is_empty(), "no blank lines in a clean file");
    }
}
