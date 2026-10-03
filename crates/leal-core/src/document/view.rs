//! A row as it reads now: the file's fields, with the edit overlay on top
//! (task 2.1, ADR-0008 decision 2).
//!
//! Every reader of a row (the grid, the inspector, Copy, Find, the
//! diagnostics marks, column widths and number detection) sees it through a
//! [`RowView`], so they all agree on what each cell holds. A row with no
//! edits is its parsed fields, as before: the view only adds a check that
//! there are no edits and no column operations. An edited row's cells are,
//! column by column, an edited value, one of the row's own fields, or an
//! empty cell between the row's end and an edited cell past it (a hatched
//! cell, ADR-0005 decision 2).
//!
//! An inserted row (task 2.4a) is a view too: its own values instead of
//! the file's fields, with its edits on top. Column inserts and deletes
//! (task 2.4b) decide which cell each logical column is (the row's layout,
//! `edit::Layout`), here, so the readers of one row don't change: a cell a
//! column insert gave the row reads as the inserted value until edited.
//! Which row a logical row is isn't decided here: every walk over rows goes
//! through the piece list (`RowMap::segments`).

use std::borrow::Cow;
use std::sync::Arc;

use crate::diagnostics::{DiagnosticKind, field_has, value_has};
use crate::edit::{
    CellId, Columns, Fold, InsertedRow, Kinds, Layout, OverlayRow, Own, RowEdits, RowId,
};
use crate::rows::{FieldSpan, ParsedRow, RowParser};

/// One cell of a [`RowView`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ViewCell<'a> {
    /// One of the row's own fields, unedited.
    Field(&'a FieldSpan),
    /// An edited value.
    Edited(&'a str),
    /// A new cell, unedited: one of an inserted row's own values, or the
    /// value a column insert gave the row.
    New(&'a str),
    /// A cell between the end of the row's own fields and an edited cell
    /// past them: empty, with no bytes in the file.
    Padding,
}

/// A row's own cells: the file's fields, or an inserted row's values.
#[derive(Clone, Copy, Debug)]
enum OwnCells<'a> {
    Parsed(&'a ParsedRow),
    New(&'a [Arc<str>]),
}

impl OwnCells<'_> {
    fn len(&self) -> usize {
        match self {
            OwnCells::Parsed(row) => row.fields().len(),
            OwnCells::New(values) => values.len(),
        }
    }
}

/// No column operations: for a view of a row's own fields alone.
static NO_COLUMNS: Columns = Columns::NONE;

/// A row as it reads now. `bytes` are the file's bytes from `base` on, and
/// hold the row (none, for an inserted row).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowView<'a> {
    parser: &'a RowParser,
    bytes: &'a [u8],
    base: usize,
    own: OwnCells<'a>,
    id: RowId,
    edits: Option<&'a RowEdits>,
    columns: &'a Columns,
}

impl<'a> RowView<'a> {
    /// A row of the file, `cells` saying its id, its edits and the column
    /// operations ([`Overlay::cells_of`](crate::edit::Overlay::cells_of)).
    pub(crate) fn new(
        parser: &'a RowParser,
        bytes: &'a [u8],
        base: usize,
        row: &'a ParsedRow,
        cells: OverlayRow<'a>,
    ) -> RowView<'a> {
        RowView {
            parser,
            bytes,
            base,
            own: OwnCells::Parsed(row),
            id: cells.id,
            edits: cells.edits,
            columns: cells.columns,
        }
    }

    /// Row `id` of the file as the file has it: no edits, no column
    /// operations.
    pub(crate) fn own(
        parser: &'a RowParser,
        bytes: &'a [u8],
        base: usize,
        row: &'a ParsedRow,
        id: RowId,
    ) -> RowView<'a> {
        RowView {
            parser,
            bytes,
            base,
            own: OwnCells::Parsed(row),
            id,
            edits: None,
            columns: &NO_COLUMNS,
        }
    }

    /// Inserted row `row`, with `cells`.
    pub(crate) fn inserted(
        parser: &'a RowParser,
        row: &'a InsertedRow,
        cells: OverlayRow<'a>,
    ) -> RowView<'a> {
        RowView {
            parser,
            bytes: &[],
            base: 0,
            own: OwnCells::New(row.fields()),
            id: cells.id,
            edits: cells.edits,
            columns: cells.columns,
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

    /// The row's id.
    pub(crate) fn id(&self) -> RowId {
        self.id
    }

    /// The row as the file has it, if it is a row of the file with no
    /// edits and no column operations: what the readers' fast paths take.
    pub(crate) fn plain(&self) -> Option<&'a ParsedRow> {
        match (self.own, self.edits) {
            (OwnCells::Parsed(row), None) if self.columns.is_empty() => Some(row),
            _ => None,
        }
    }

    /// What the default layout needs to know of the row.
    fn own_shape(&self) -> Own {
        let fields = self.own.len();
        match self.own {
            OwnCells::Parsed(row) => {
                let blank = fields == 1 && row.span().is_empty();
                Own::original(fields, blank)
            }
            OwnCells::New(_) => Own::inserted(self.id.inserted_index().unwrap_or(0), fields),
        }
    }

    /// Which cell each logical column is.
    fn layout(&self) -> Layout<'a> {
        Layout::of(self.columns, self.own_shape(), self.edits)
    }

    /// How many cells the row has now: its own fields as read (and the
    /// cells column inserts gave it, less those deletes took), or up to its
    /// last edited cell, whichever is more. (Not the field count stored
    /// with the edits: the row may have been edited from the first 64 KB
    /// and now be read from the trusted copy, task 1.9.)
    pub(crate) fn len(&self) -> usize {
        if self.edits.is_none() && self.columns.is_empty() {
            return self.own.len();
        }
        self.layout().len()
    }

    /// Which cell logical column `column` is, or `None` past the row's
    /// end: for the writer (task 2.4c), which copies a field's bytes and
    /// writes everything else.
    pub(crate) fn cell_id(&self, column: usize) -> Option<CellId> {
        self.layout().get(column)
    }

    /// Whether the row's cells are all its own fields in their places,
    /// then hatched cells: no column operation moved or took anything in
    /// it, so it can be written field by field (task 2.4c).
    pub(crate) fn same_shape(&self) -> bool {
        let fields = self.own.len();
        match self.layout() {
            Layout::Default { fold, .. } => match fold {
                Fold::Identity { .. } => true,
                fold => {
                    fold.len() == fields
                        && (0..fields).all(|k| fold.get(k) == Some(CellId::base(k, fields)))
                }
            },
            // Every field still there (a delete may have taken its last
            // one, padding and all), in its place.
            Layout::Explicit(ids) => {
                ids.len() >= fields
                    && ids.iter().enumerate().all(|(k, &id)| match id {
                        CellId::Field(f) => usize::try_from(f).is_ok_and(|f| f == k),
                        CellId::Appended(_) => k >= fields,
                        CellId::Inserted(_) => false,
                    })
            }
        }
    }

    /// Whether the row reads as a blank line of the file: its one cell is
    /// its own empty field, unedited (a column delete can make an edited
    /// blank line one again). Column operations never change one.
    pub(crate) fn is_blank_line(&self) -> bool {
        let OwnCells::Parsed(row) = self.own else {
            return false;
        };
        let blank = row.fields().len() == 1 && row.span().is_empty();
        blank
            && (self.edits.is_none()
                || (self.len() == 1
                    && self.cell_id(0) == Some(CellId::Field(0))
                    && matches!(self.cell(0), Some(ViewCell::Field(_)))))
    }

    /// The row's own fields still in it, edited or not, with their logical
    /// columns, in order: what per-column quoting judges, by their original
    /// bytes (ADR-0014 decision 4). None for an inserted row.
    pub(crate) fn fields_now(&self) -> Vec<(usize, &'a FieldSpan)> {
        let OwnCells::Parsed(row) = self.own else {
            return Vec::new();
        };
        let field = |id: CellId| match id {
            CellId::Field(k) => usize::try_from(k).ok().and_then(|k| row.field(k)),
            CellId::Inserted(_) | CellId::Appended(_) => None,
        };
        match self.layout() {
            Layout::Default { fold, .. } => {
                let folded = match &fold {
                    Fold::Identity { len, .. } => row.fields().len().min(*len),
                    fold => fold.len(),
                };
                (0..folded)
                    .filter_map(|c| Some((c, field(fold.get(c)?)?)))
                    .collect()
            }
            Layout::Explicit(ids) => ids
                .iter()
                .enumerate()
                .filter_map(|(c, &id)| Some((c, field(id)?)))
                .collect(),
        }
    }

    /// The row's own fields as the file has them, whatever its edits: none
    /// for an inserted row.
    pub(crate) fn file_fields(&self) -> Option<&'a [FieldSpan]> {
        match self.own {
            OwnCells::Parsed(row) => Some(row.fields()),
            OwnCells::New(_) => None,
        }
    }

    /// The row's own cell `k`, unedited, if it has one.
    fn own_cell(&self, k: usize) -> Option<ViewCell<'a>> {
        match self.own {
            OwnCells::Parsed(row) => row.field(k).map(ViewCell::Field),
            OwnCells::New(values) => values.get(k).map(|value| ViewCell::New(value)),
        }
    }

    /// The cell `id` is, as it reads now.
    fn cell_of(&self, id: CellId) -> ViewCell<'a> {
        if let Some(value) = self.edits.and_then(|edits| edits.get(id)) {
            return ViewCell::Edited(value);
        }
        match id {
            // A hatched cell is padding, unless the row as read now is
            // longer than when it was edited (task 1.9's stale first 64 KB).
            CellId::Field(k) | CellId::Appended(k) => usize::try_from(k)
                .ok()
                .and_then(|k| self.own_cell(k))
                .unwrap_or(ViewCell::Padding),
            CellId::Inserted(op) => self
                .columns
                .inserted_value(op, self.id)
                .map_or(ViewCell::Padding, ViewCell::New),
        }
    }

    /// Cell `column`, or `None` past the row's end.
    pub(crate) fn cell(&self, column: usize) -> Option<ViewCell<'a>> {
        if self.edits.is_none() && self.columns.is_empty() {
            return self.own_cell(column);
        }
        Some(self.cell_of(self.layout().get(column)?))
    }

    /// The row's cells that hold something, in column order: its own
    /// fields, unedited, its inserted cells and its edited cells, but not
    /// the padding between its end and an edited cell past it, which is
    /// empty and has no diagnostics. So a far hatched edit (up to
    /// [`COLUMN_LIMIT`]) costs what the row's fields and edits do, not a
    /// cell per column.
    ///
    /// [`COLUMN_LIMIT`]: crate::edit::COLUMN_LIMIT
    pub(crate) fn filled(&self) -> Vec<(usize, ViewCell<'a>)> {
        if self.edits.is_none() && self.columns.is_empty() {
            return match self.own {
                OwnCells::Parsed(row) => row
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(column, field)| (column, ViewCell::Field(field)))
                    .collect(),
                OwnCells::New(values) => values
                    .iter()
                    .enumerate()
                    .map(|(column, value)| (column, ViewCell::New(value)))
                    .collect(),
            };
        }
        let layout = self.layout();
        let mut cells: Vec<(usize, ViewCell<'a>)> = Vec::new();
        let mut keep = |column: usize, id: CellId| {
            let cell = self.cell_of(id);
            if cell != ViewCell::Padding {
                cells.push((column, cell));
            }
        };
        match &layout {
            Layout::Default { fold, .. } => {
                // The folded cells, but an identity's long run of padding
                // only where something is: its own fields and edits.
                let folded = match fold {
                    Fold::Identity { len, .. } => self.own.len().min(*len),
                    fold => fold.len(),
                };
                for column in 0..folded {
                    if let Some(id) = fold.get(column) {
                        keep(column, id);
                    }
                }
                if let Some(edits) = self.edits {
                    for &(id, _) in edits.cells() {
                        let column = match (fold, id) {
                            (Fold::Identity { .. }, CellId::Field(k) | CellId::Appended(k)) => {
                                usize::try_from(k).ok().filter(|&c| c >= folded)
                            }
                            (_, CellId::Appended(j)) => usize::try_from(j)
                                .ok()
                                .and_then(|j| layout.tail_position(j)),
                            _ => None,
                        };
                        if let Some(column) = column {
                            keep(column, id);
                        }
                    }
                }
            }
            Layout::Explicit(ids) => {
                for (column, &id) in ids.iter().enumerate() {
                    keep(column, id);
                }
            }
        }
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
        if let Some(row) = self.plain() {
            // A row of the file with no edits: its fields, in order, with
            // nothing to collect first.
            return row
                .fields()
                .iter()
                .position(|field| self.cell_has(kind, ViewCell::Field(field)));
        }
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
        let OwnCells::Parsed(row) = self.own else {
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
