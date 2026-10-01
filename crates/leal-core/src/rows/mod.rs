//! Rows and fields: the lenient row parser, display values and the cache
//! of parsed rows (DESIGN §3.4).
//!
//! The [row index](crate::index) says where each row is. This module splits
//! one row into fields, only when the row is needed (to draw it, copy it,
//! and so on), and turns a field's bytes into the text the grid shows.
//!
//! ```
//! use leal_core::index::{CodeUnit, IndexDialect, RowIndex};
//! use leal_core::rows::{Encoding, RowCache, RowParser};
//!
//! let bytes = b"id,name\n1,\"Smith, \"\"Jo\"\"\"\n2,\"a\"b\n";
//! let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
//! let index = RowIndex::build(bytes, dialect)?;
//! let parser = RowParser::new(dialect, Encoding::Utf8)?;
//!
//! let row = parser.parse_row(&index, 1, bytes).unwrap();
//! let name = &row.fields()[1];
//! assert_eq!(name.span(), 10..25);
//! assert!(name.quoted());
//! assert_eq!(parser.display_value(bytes, name), "Smith, \"Jo\"");
//!
//! // Text after a closing quote is shown raw (ADR-0003 decision 2).
//! let row = parser.parse_row(&index, 2, bytes).unwrap();
//! assert_eq!(parser.display_value(bytes, &row.fields()[1]), "\"a\"b");
//!
//! // The grid reads rows through a small cache.
//! let mut cache = RowCache::new(parser, 256);
//! let row = cache.row(&index, 1, bytes).unwrap();
//! assert_eq!(row.fields().len(), 2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Rules
//!
//! DESIGN §3.4 and ADR-0003, as the testkit's generator and the corpus
//! sidecars pin them. The row's span comes from [`RowIndex::row`], so it
//! excludes the line ending.
//!
//! - A quote is special only as the **first unit of a field**. Anywhere
//!   else it is literal text: `a"b` is the value `a"b`.
//! - Inside a quoted field, `""` is an escaped quote, and any other quote
//!   closes the field. Delimiters, CR and LF inside are literal.
//! - Units between a closing quote and the next delimiter (`"a"b,`) belong
//!   to the field. Quotes among them are literal (ADR-0003 decision 3), so
//!   `"a"b"c",` is one field with raw bytes `"a"b"c"`.
//! - A quoted field whose quote never closes runs to the end of the row,
//!   which is the end of the file.
//! - A blank row has one empty, unquoted field.
//!
//! Every position is a **byte offset into the file as stored**, the BOM
//! included, in UTF-16 too (ADR-0003 decision 6).
//!
//! # Display values
//!
//! [`RowParser::display_value`] derives the text from the field's bytes:
//!
//! - unquoted: the raw bytes;
//! - quoted: the bytes between the quotes, with `""` turned into `"`;
//! - quoted with text after the closing quote: the raw bytes, quotes and
//!   all, so `"a"b` shows as `"a"b` (ADR-0002 question 6, ADR-0003
//!   decision 2);
//! - unterminated: everything after the opening quote, with `""` turned
//!   into `"`;
//!
//! then decoded in the file's [`Encoding`]. Invalid UTF-8, an unpaired
//! UTF-16 surrogate (ADR-0003 decision 7), a final odd byte in UTF-16 and
//! a byte a single-byte encoding doesn't map all display as U+FFFD. The
//! result borrows from the file when nothing needs changing, which is
//! almost always for UTF-8 (see `Cow` in the 1.4 notes).
//!
//! The grid shows only the start of a cell, so it reads cells with
//! [`RowParser::display_prefix`], whose work is proportional to the
//! characters asked for, not to the field's length. A 1 MB field costs no
//! more to draw than a short one. The cell inspector, which shows all of a
//! value, uses [`RowParser::display_value`].
//!
//! # The seam with detection (1.2) and the scheduler (1.3a)
//!
//! The parser takes the [`IndexDialect`] the index was built with and an
//! [`Encoding`]. That [`Encoding`] is a provisional local copy of the set
//! in ADR-0005 decision 5, with the same variant names as 1.2's
//! `dialect::Encoding`, because 1.2 isn't on `main` yet. 1.3a should
//! replace it with 1.2's and pass the detected encoding here, as it passes
//! the detected delimiter and BOM to the index.
//!
//! First paint (1.3a) doesn't wait for the index. It can index the first
//! 64 KB on their own (`RowIndex::build` over that slice), drop the last
//! row, which may be cut off, and parse the rest: their spans are the same
//! offsets as in the whole file.

mod cache;
mod display;
mod encoding;
mod parse;
#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;

use crate::index::{CodeUnit, IndexDialect, RowIndex};

pub use cache::{DEFAULT_CACHE_FIELDS, DEFAULT_CACHE_ROWS, RowCache};
pub use encoding::Encoding;

/// One field of a parsed row: where its raw bytes are, and how it is
/// quoted.
///
/// The span covers the field's quotes, escapes and any text after its
/// closing quote, but not the delimiter after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSpan {
    start: usize,
    len: usize,
    kind: FieldKind,
}

/// How a field is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldKind {
    /// It doesn't start with a quote.
    Unquoted,
    /// `"…"`, closed, with nothing after the closing quote.
    Quoted,
    /// `"…"…`: closed, then more units before the delimiter. The offset is
    /// the first unit after the closing quote.
    TextAfterQuote(usize),
    /// `"…` with no closing quote: it runs to the end of the file.
    Unterminated,
}

impl FieldSpan {
    /// The offset of the field's first byte.
    #[must_use]
    pub fn start(&self) -> usize {
        self.start
    }

    /// The length of the field's raw bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True if the field has no bytes at all (`a,,b`, or a blank row).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The field's raw bytes, as a range of offsets into the file.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        self.start..self.start + self.len
    }

    /// True if the field's first unit is the quote character.
    #[must_use]
    pub fn quoted(&self) -> bool {
        !matches!(self.kind, FieldKind::Unquoted)
    }

    /// The offset of the first unit after the closing quote, if the field
    /// has text between its closing quote and the next delimiter
    /// (`"a"b`). Diagnostics (1.5) report it there.
    #[must_use]
    pub fn text_after_quote(&self) -> Option<usize> {
        match self.kind {
            FieldKind::TextAfterQuote(at) => Some(at),
            _ => None,
        }
    }

    /// True if the field's opening quote never closes. Such a field runs to
    /// the end of the file.
    #[must_use]
    pub fn unterminated(&self) -> bool {
        matches!(self.kind, FieldKind::Unterminated)
    }

    /// The field's raw bytes in `bytes`, the file it was parsed from. Empty
    /// if `bytes` is too short to hold them (the wrong file).
    #[must_use]
    pub fn raw<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        bytes.get(self.span()).unwrap_or_default()
    }
}

/// One row, split into fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedRow {
    span: Range<usize>,
    fields: Vec<FieldSpan>,
}

impl ParsedRow {
    /// The row's bytes, excluding its line ending.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        self.span.clone()
    }

    /// The row's fields, in order. There is always at least one.
    #[must_use]
    pub fn fields(&self) -> &[FieldSpan] {
        &self.fields
    }

    /// Field `field`, or `None` if the row has fewer fields (a short,
    /// ragged row).
    #[must_use]
    pub fn field(&self, field: usize) -> Option<&FieldSpan> {
        self.fields.get(field)
    }
}

/// Why a [`RowParser`] couldn't be made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowsError {
    /// The delimiter and quote must be different ASCII characters, and
    /// neither may be CR or LF (the same rule as the index's).
    InvalidDialect {
        /// The delimiter given.
        delimiter: u8,
        /// The quote given.
        quote: u8,
    },
    /// The dialect's code unit doesn't match the encoding: UTF-16 needs
    /// UTF-16 code units, and every other encoding needs bytes.
    EncodingMismatch {
        /// The dialect's code unit.
        code_unit: CodeUnit,
        /// The encoding given.
        encoding: Encoding,
    },
}

impl fmt::Display for RowsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowsError::InvalidDialect { delimiter, quote } => write!(
                f,
                "can't parse rows with delimiter {:?} and quote {:?}: they must be different ASCII characters other than CR and LF",
                char::from(*delimiter),
                char::from(*quote)
            ),
            RowsError::EncodingMismatch {
                code_unit,
                encoding,
            } => write!(
                f,
                "a file read as {encoding:?} can't be parsed in {code_unit:?} code units"
            ),
        }
    }
}

impl std::error::Error for RowsError {}

/// Splits rows into fields and derives display values, for one dialect
/// and encoding. Cheap to copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowParser {
    dialect: IndexDialect,
    encoding: Encoding,
}

impl RowParser {
    /// A parser for files indexed with `dialect` and read as `encoding`.
    ///
    /// # Errors
    ///
    /// [`RowsError::InvalidDialect`] or [`RowsError::EncodingMismatch`].
    pub fn new(dialect: IndexDialect, encoding: Encoding) -> Result<RowParser, RowsError> {
        let IndexDialect {
            delimiter, quote, ..
        } = dialect;
        let usable = |b: u8| b.is_ascii() && b != b'\r' && b != b'\n';
        if !usable(delimiter) || !usable(quote) || delimiter == quote {
            return Err(RowsError::InvalidDialect { delimiter, quote });
        }
        if encoding.code_unit() != dialect.code_unit {
            return Err(RowsError::EncodingMismatch {
                code_unit: dialect.code_unit,
                encoding,
            });
        }
        Ok(RowParser { dialect, encoding })
    }

    /// The dialect this parser splits rows with.
    #[must_use]
    pub fn dialect(&self) -> IndexDialect {
        self.dialect
    }

    /// The encoding display values are decoded from.
    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Splits the row whose bytes are `span` (excluding its line ending, as
    /// [`RowIndex::row`] gives it) into fields.
    ///
    /// Returns `None` if `span` isn't inside `bytes` after the BOM, or, in
    /// UTF-16, doesn't start and end on whole code units (a final odd byte
    /// at the end of the file is allowed).
    #[must_use]
    pub fn parse(&self, bytes: &[u8], span: Range<usize>) -> Option<ParsedRow> {
        parse::parse(self.dialect, bytes, span)
    }

    /// Row `row` of the file, split into fields, or `None` if the index
    /// doesn't have that row (yet), or was built with another dialect.
    /// `bytes` must be the whole file that was indexed.
    #[must_use]
    pub fn parse_row(&self, index: &RowIndex, row: usize, bytes: &[u8]) -> Option<ParsedRow> {
        if index.dialect() != self.dialect {
            return None;
        }
        let span = index.row(row, bytes)?.span;
        self.parse(bytes, span)
    }
}
