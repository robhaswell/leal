//! A value a row or column command puts back by value (ADR-0014 decision
//! 3, task 2.4c): text, or one of the file's fields as its bytes, so a
//! field whose bytes aren't text in the file's encoding (or that is quoted
//! its own way) comes back byte for byte.

use std::sync::Arc;

use super::overlay::Kinds;
use crate::rows::FieldSpan;

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
/// field they parse as (from offset `base`), how it reads, and its
/// field-level diagnostics. Never an unterminated quote's field, which would swallow
/// what follows it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RawField {
    bytes: Box<[u8]>,
    /// The offset `bytes` start at, for `field` (past any BOM, as a row
    /// parser wants).
    base: usize,
    field: FieldSpan,
    text: Arc<str>,
    kinds: Kinds,
}

impl RawField {
    /// A field of `bytes`, which (starting at offset `base`) parse as
    /// `field`, reading as `text`, with the diagnostics `kinds`.
    pub(crate) fn new(
        bytes: Box<[u8]>,
        base: usize,
        field: FieldSpan,
        text: &str,
        kinds: Kinds,
    ) -> RawField {
        RawField {
            bytes,
            base,
            field,
            text: Arc::from(text),
            kinds,
        }
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
