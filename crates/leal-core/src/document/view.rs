//! A row as it reads now: the file's fields, with the edit overlay on top
//! (task 2.1, ADR-0008 decision 2).
//!
//! Every reader of a row (the grid, the inspector, Copy, Find, the
//! diagnostics marks, column widths and number detection) sees it through a
//! [`RowView`], so they all agree on what each cell holds. A row with no
//! edits is its parsed fields, as before: the view only adds a check that
//! there are no edits. An edited row's cells are, column by column, an
//! edited value, one of the row's own fields, or an empty cell between the
//! row's end and an edited cell past it (a hatched cell, ADR-0005
//! decision 2).
//!
//! An inserted row (task 2.4a) is a view too: its own values instead of
//! the file's fields, with its edits on top. Task 2.4b's column inserts and
//! deletes change which field a logical cell comes from, which is decided
//! here, so the readers of one row don't change. Which row a logical row is
//! isn't decided here: every walk over rows goes through the piece list
//! (`RowMap::segments`).

use std::borrow::Cow;
use std::sync::Arc;

use crate::diagnostics::{DiagnosticKind, field_has, value_has};
use crate::edit::{InsertedRow, Kinds, RowEdits};
use crate::rows::{FieldSpan, ParsedRow, RowParser};

/// One cell of a [`RowView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewCell<'a> {
    /// One of the row's own fields, unedited.
    Field(&'a FieldSpan),
    /// An edited value.
    Edited(&'a str),
    /// One of an inserted row's own values, unedited.
    New(&'a str),
    /// A cell between the end of the row's own fields and an edited cell
    /// past them: empty, with no bytes in the file.
    Padding,
}

/// A row's own cells: the file's fields, or an inserted row's values.
#[derive(Clone, Copy, Debug)]
enum Own<'a> {
    Parsed(&'a ParsedRow),
    New(&'a [Arc<str>]),
}

impl Own<'_> {
    fn len(&self) -> usize {
        match self {
            Own::Parsed(row) => row.fields().len(),
            Own::New(values) => values.len(),
        }
    }
}

/// A row as it reads now. `bytes` are the file's bytes from `base` on, and
/// hold the row (none, for an inserted row).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowView<'a> {
    parser: &'a RowParser,
    bytes: &'a [u8],
    base: usize,
    own: Own<'a>,
    edits: Option<&'a RowEdits>,
}

impl<'a> RowView<'a> {
    pub(crate) fn new(
        parser: &'a RowParser,
        bytes: &'a [u8],
        base: usize,
        row: &'a ParsedRow,
        edits: Option<&'a RowEdits>,
    ) -> RowView<'a> {
        RowView {
            parser,
            bytes,
            base,
            own: Own::Parsed(row),
            edits,
        }
    }

    /// Inserted row `row`, with `edits`.
    pub(crate) fn inserted(
        parser: &'a RowParser,
        row: &'a InsertedRow,
        edits: Option<&'a RowEdits>,
    ) -> RowView<'a> {
        RowView {
            parser,
            bytes: &[],
            base: 0,
            own: Own::New(row.fields()),
            edits,
        }
    }

    pub(crate) fn parser(&self) -> &'a RowParser {
        self.parser
    }

    pub(crate) fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub(crate) fn base(&self) -> usize {
        self.base
    }

    /// The row as the file has it, if it is a row of the file with no
    /// edits: what the readers' fast paths take.
    pub(crate) fn plain(&self) -> Option<&'a ParsedRow> {
        match (self.own, self.edits) {
            (Own::Parsed(row), None) => Some(row),
            _ => None,
        }
    }

    /// How many cells the row has now: its own fields as read, or up to
    /// its last edited cell, whichever is more. (Not the field count stored
    /// with the edits: the row may have been edited from the first 64 KB
    /// and now be read from the trusted copy, task 1.9.)
    pub(crate) fn len(&self) -> usize {
        let fields = self.own.len();
        self.edits.map_or(fields, |edits| fields.max(edits.end()))
    }

    /// The row's own cell `column`, unedited, if it has one.
    fn own(&self, column: usize) -> Option<ViewCell<'a>> {
        match self.own {
            Own::Parsed(row) => row.field(column).map(ViewCell::Field),
            Own::New(values) => values.get(column).map(|value| ViewCell::New(value)),
        }
    }

    /// Cell `column`, or `None` past the row's end.
    pub(crate) fn cell(&self, column: usize) -> Option<ViewCell<'a>> {
        let Some(edits) = self.edits else {
            return self.own(column);
        };
        if let Some(value) = edits.get(column) {
            return Some(ViewCell::Edited(value));
        }
        match self.own(column) {
            Some(cell) => Some(cell),
            None if column < edits.end() => Some(ViewCell::Padding),
            None => None,
        }
    }

    /// The row's cells that hold something, in column order: its own
    /// fields, unedited, and its edited cells, but not the padding between
    /// its end and an edited cell past it, which is empty and has no
    /// diagnostics. So a far hatched edit (up to [`COLUMN_LIMIT`]) costs
    /// what the row's fields and edits do, not a cell per column.
    ///
    /// [`COLUMN_LIMIT`]: crate::edit::COLUMN_LIMIT
    pub(crate) fn filled(&self) -> Vec<(usize, ViewCell<'a>)> {
        let own = (0..self.own.len()).filter_map(|column| Some((column, self.own(column)?)));
        let Some(edits) = self.edits else {
            return own.collect();
        };
        let mut cells: Vec<(usize, ViewCell<'a>)> = own
            .filter(|&(column, _)| edits.get(column).is_none())
            .collect();
        cells.extend(
            edits
                .cells()
                .iter()
                .map(|(column, value)| (*column, ViewCell::Edited(value))),
        );
        cells.sort_unstable_by_key(|&(column, _)| column);
        cells
    }

    /// The first `max_chars` characters of cell `column`'s display value,
    /// and whether it has more (as [`RowParser::display_prefix_in`]), or
    /// `None` past the row's end.
    pub(crate) fn prefix(&self, column: usize, max_chars: usize) -> Option<(Cow<'a, str>, bool)> {
        Some(self.prefix_of(self.cell(column)?, max_chars))
    }

    /// [`prefix`](Self::prefix), of a cell of this row.
    pub(crate) fn prefix_of(&self, cell: ViewCell<'a>, max_chars: usize) -> (Cow<'a, str>, bool) {
        match cell {
            ViewCell::Field(field) => self
                .parser
                .display_prefix_in(self.bytes, self.base, field, max_chars),
            ViewCell::Edited(value) | ViewCell::New(value) => {
                match value.char_indices().nth(max_chars) {
                    Some((cut, _)) => (Cow::Borrowed(&value[..cut]), true),
                    None => (Cow::Borrowed(value), false),
                }
            }
            ViewCell::Padding => (Cow::Borrowed(""), false),
        }
    }

    /// Cell `column`'s whole display value, or `None` past the row's end.
    pub(crate) fn value(&self, column: usize) -> Option<Cow<'a, str>> {
        Some(self.value_of(self.cell(column)?))
    }

    /// [`value`](Self::value), of a cell of this row.
    pub(crate) fn value_of(&self, cell: ViewCell<'a>) -> Cow<'a, str> {
        match cell {
            ViewCell::Field(field) => self.parser.display_value_in(self.bytes, self.base, field),
            ViewCell::Edited(value) | ViewCell::New(value) => Cow::Borrowed(value),
            ViewCell::Padding => Cow::Borrowed(""),
        }
    }

    /// Whether `cell` has an occurrence of the field-level `kind`: a field
    /// from its bytes, as the index pass decides it, and an edited value on
    /// the value itself (ADR-0008 decision 2), where only a NUL can be.
    pub(crate) fn cell_has(&self, kind: DiagnosticKind, cell: ViewCell<'_>) -> bool {
        match cell {
            ViewCell::Field(field) => {
                field_has(kind, self.parser.encoding(), self.bytes, self.base, field)
            }
            ViewCell::Edited(value) | ViewCell::New(value) => value_has(kind, value),
            ViewCell::Padding => false,
        }
    }

    /// The first cell with an occurrence of the field-level `kind`.
    pub(crate) fn first_with(&self, kind: DiagnosticKind) -> Option<usize> {
        self.filled()
            .into_iter()
            .find(|&(_, cell)| self.cell_has(kind, cell))
            .map(|(column, _)| column)
    }

    /// The row's own fields that have field-level diagnostics, and which,
    /// from their bytes as the index pass decides them: for its edits'
    /// marks, worked out once, when the row is first edited.
    pub(crate) fn flagged_fields(&self) -> Vec<(usize, Kinds)> {
        let mut flagged = Vec::new();
        let Own::Parsed(row) = self.own else {
            return flagged;
        };
        for (column, field) in row.fields().iter().enumerate() {
            let mut kinds = Kinds::default();
            for &(kind, bit) in &Kinds::FIELD_KINDS {
                if self.cell_has(kind, ViewCell::Field(field)) {
                    kinds.insert(bit);
                }
            }
            if !kinds.is_empty() {
                flagged.push((column, kinds));
            }
        }
        flagged
    }
}
