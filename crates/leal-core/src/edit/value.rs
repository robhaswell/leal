//! A value a row or column command puts back by value (ADR-0014 decision
//! 3, task 2.4c): text, or one of the file's fields as its bytes, so a
//! field whose bytes aren't text in the file's encoding (or that is quoted
//! its own way) comes back byte for byte.

use std::sync::Arc;

use super::overlay::Kinds;
use crate::diagnostics::field_has;
use crate::dialect::Encoding;
use crate::rows::{FieldSpan, RowParser};
use crate::save::Transcoder;

/// A cell's value as an inserted row, or a column put back, holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    /// Text: typed, inserted, or a field's that can't come back as bytes.
    Text(Arc<str>),
    /// One of the file's fields, as its bytes.
    Raw(Arc<RawField>),
}

impl Value {
    /// How it reads.
    pub(crate) fn text(&self) -> &str {
        match self {
            Value::Text(text) => text,
            Value::Raw(raw) => raw.text(),
        }
    }

    /// The field's bytes, if it is one.
    pub(crate) fn raw(&self) -> Option<&RawField> {
        match self {
            Value::Text(_) => None,
            Value::Raw(raw) => Some(raw),
        }
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Value {
        Value::Text(Arc::from(text))
    }
}

/// One of a file's fields, put back by value: its bytes as written
/// (quotes, escapes and any text after the closing quote included), the
/// field they parse as (from offset `base`), how it reads, its field-level
/// diagnostics, and the encoding, delimiter and quote it was read under.
/// Never an unterminated quote's field, which would swallow what follows
/// it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawField {
    bytes: Box<[u8]>,
    /// The offset `bytes` start at, for `field` (past any BOM, as a row
    /// parser wants).
    base: usize,
    field: FieldSpan,
    text: Arc<str>,
    kinds: Kinds,
    read_as: ReadAs,
}

/// What a field's bytes are read under: whether another reading reads
/// them the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadAs {
    encoding: Encoding,
    delimiter: u8,
    quote: u8,
}

impl ReadAs {
    pub(crate) fn of(parser: &RowParser) -> ReadAs {
        let dialect = parser.dialect();
        ReadAs {
            encoding: parser.encoding(),
            delimiter: dialect.delimiter,
            quote: dialect.quote,
        }
    }
}

impl Value {
    /// The value as it goes into a reading `parser` reads (task 2.4c): a
    /// field's bytes as they are if `parser` reads them as they were read,
    /// converted to UTF-8 if Save As UTF-8 came between; otherwise, or if
    /// that can't be done, its text.
    pub(crate) fn fit(&self, parser: &RowParser) -> Value {
        match self {
            Value::Text(_) => self.clone(),
            Value::Raw(raw) => RawField::fit(raw, parser),
        }
    }
}

impl RawField {
    /// A field's bytes `raw` (one whole field, with no line ending), read
    /// under `from`, as a field `to` reads the same way: the same
    /// delimiter and quote, and the same encoding or UTF-8 converted from
    /// it. `None` if it can't be (another dialect or encoding, bytes that
    /// aren't text, converted, or that don't parse as one whole field).
    pub(crate) fn read(raw: &[u8], from: ReadAs, to: &RowParser) -> Option<RawField> {
        let into = ReadAs::of(to);
        if (from.delimiter, from.quote) != (into.delimiter, into.quote) {
            return None;
        }
        let bytes: Box<[u8]> = if from.encoding == into.encoding {
            Box::from(raw)
        } else if into.encoding == Encoding::Utf8 {
            Transcoder::convert(raw, from.encoding)?
                .into_owned()
                .into_boxed_slice()
        } else {
            return None;
        };
        let base = to.dialect().bom_len;
        let span = base..base + bytes.len();
        let parsed = to.parse_in(&bytes, base, span.clone())?;
        let [field] = parsed.fields() else {
            return None;
        };
        if field.span() != span || field.unterminated() {
            return None;
        }
        let mut kinds = Kinds::default();
        for &(kind, bit) in &Kinds::FIELD_KINDS {
            if field_has(kind, into.encoding, &bytes, base, field) {
                kinds.insert(bit);
            }
        }
        let text = Arc::from(to.display_value_in(&bytes, base, field).as_ref());
        Some(RawField {
            bytes,
            base,
            field: *field,
            text,
            kinds,
            read_as: into,
        })
    }

    /// `raw` as it goes into a reading `parser` reads ([`Value::fit`]).
    pub(crate) fn fit(raw: &Arc<RawField>, parser: &RowParser) -> Value {
        if raw.read_as == ReadAs::of(parser) {
            return Value::Raw(Arc::clone(raw));
        }
        RawField::read(raw.field_bytes(), raw.read_as, parser).map_or_else(
            || Value::Text(Arc::clone(&raw.text)),
            |fitted| Value::Raw(Arc::new(fitted)),
        )
    }

    /// The field's own bytes, without any before its start.
    fn field_bytes(&self) -> &[u8] {
        let start = self.field.start() - self.base;
        &self.bytes[start..start + self.field.len()]
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn base(&self) -> usize {
        self.base
    }

    pub(crate) fn field(&self) -> &FieldSpan {
        &self.field
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn kinds(&self) -> Kinds {
        self.kinds
    }
}
