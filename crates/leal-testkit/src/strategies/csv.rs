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
        let diagnostics = diagnostics::derive(&layout, &bytes, encoding == Encoding::Utf8);
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
        assert!(bad(vec![row(vec![unq("\u{FEFF}a")], None)]).is_err());
        assert!(
            bad(vec![
                row(vec![unq("a")], Some(LineEnding::Cr)),
                row(vec![unq("")], Some(LineEnding::Lf)),
            ])
            .is_err()
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
