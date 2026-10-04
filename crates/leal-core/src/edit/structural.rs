//! What a row insert or delete command carries (task 2.4a,
//! `docs/tasks/2.4.md` §4): the rows, by identity, for an undo or redo in
//! the same edits, and a way to read their values, for one in other edits
//! (after a save, or in a replay).

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::columns::{CellId, ColumnOp, Columns, Layout, OpKind, Own};
use super::overlay::{InsertedRow, Overlay, RowEdits};
use super::rows::{Piece, RowId};
use super::value::Value;
use crate::rows::RowParser;
use crate::source::ReadError;

/// Which file a document's row ids are of: a new one for each
/// [`EditStore`](super::EditStore), so for each split of the file and each
/// save's rebase (ADR-0014 decision 3). A structural command made in the
/// same base is undone and redone by identity, so original bytes come
/// back; one from another works by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BaseId(u64);

impl BaseId {
    pub(crate) fn new() -> BaseId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        BaseId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// Where a command reads its original rows' values: the reading of the
/// file it was made on, kept alive by the command (ADR-0014 decision 3).
pub(crate) trait RowSource: Send + Sync {
    /// Physical rows `rows` as they read with `edits` on top, each as its
    /// cells' values: fewer if the file doesn't have them all. Unedited
    /// fields come as their bytes where `parser` (the reading they go
    /// back into) reads them the same (task 2.4c).
    fn values(
        &self,
        rows: Range<u32>,
        edits: &Overlay,
        parser: &RowParser,
    ) -> Result<Vec<Vec<Value>>, ReadError>;
}

/// The rows a row insert or delete command moves: an insert's new rows, or
/// the rows a delete took out, with their cell edits, so that undo puts the
/// same rows back. Opaque: the app holds it in the command.
pub struct Rows {
    pub(crate) base: BaseId,
    /// The rows, in logical order, as pieces.
    pub(crate) pieces: Vec<Piece>,
    pub(crate) count: usize,
    /// The inserted rows among them, by number.
    pub(crate) inserted: Vec<(u32, Arc<InsertedRow>)>,
    /// The cell edits of those of them that had some, by id.
    pub(crate) edits: Vec<(RowId, Arc<RowEdits>)>,
    /// Where the original rows among them are read from, if there are any.
    pub(crate) origin: Option<Arc<dyn RowSource>>,
    /// The column operations when the rows were deleted, which their
    /// edits were laid out under (task 2.4b).
    pub(crate) columns: Arc<Columns>,
}

impl Rows {
    /// How many rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether there are none (never, for a command a document made).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The rows' ids, in logical order, as runs.
    pub(crate) fn ids(&self) -> impl Iterator<Item = Range<RowId>> + '_ {
        self.pieces.iter().map(|piece| piece.ids())
    }

    /// Each row's cells as it read when the command was made: an inserted
    /// row's values with its edits, an original row's from the file it was
    /// read from, with its edits, its unedited fields as their bytes where
    /// `parser` reads them the same. Original rows are read now (ADR-0014
    /// decision 3). Fewer than [`len`](Self::len) if some can't be had.
    pub(crate) fn values(&self, parser: &RowParser) -> Result<Vec<Vec<Value>>, ReadError> {
        let edits: BTreeMap<RowId, &Arc<RowEdits>> =
            self.edits.iter().map(|(id, edits)| (*id, edits)).collect();
        let inserted: BTreeMap<u32, &Arc<InsertedRow>> =
            self.inserted.iter().map(|(n, row)| (*n, row)).collect();
        let mut values = Vec::with_capacity(self.count);
        for &piece in &self.pieces {
            match piece {
                Piece::Original { start, len } => {
                    let ids = piece.ids();
                    let originals = Overlay::of_rows(
                        edits
                            .range(ids)
                            .map(|(&id, &edits)| (id, Arc::clone(edits))),
                        Arc::clone(&self.columns),
                    );
                    if let Some(origin) = &self.origin {
                        values.extend(origin.values(start..start + len, &originals, parser)?);
                    }
                }
                Piece::Inserted { first, len, .. } => {
                    for n in first..first + len {
                        if let Some(row) = inserted.get(&n) {
                            let edits = edits.get(&RowId::inserted(n)).map(|e| e.as_ref());
                            let row = inserted_values(n, row, edits, &self.columns);
                            values.push(row.iter().map(|value| value.fit(parser)).collect());
                        }
                    }
                }
            }
        }
        Ok(values)
    }
}

/// Inserted row `n`'s values as it reads with `edits` under `columns`.
pub(crate) fn inserted_values(
    n: u32,
    row: &InsertedRow,
    edits: Option<&RowEdits>,
    columns: &Columns,
) -> Vec<Value> {
    let fields = row.fields();
    let layout = Layout::of(columns, Own::inserted(n, fields.len()), edits);
    let id_of_row = RowId::inserted(n);
    layout
        .ids()
        .iter()
        .map(|&id| {
            if let Some(edited) = edits.and_then(|edits| edits.get(id)) {
                return Value::from(edited);
            }
            match id {
                CellId::Field(k) => fields
                    .get(usize::try_from(k).unwrap_or(usize::MAX))
                    .cloned()
                    .unwrap_or_else(|| Value::from("")),
                CellId::Inserted(op) => match columns.op(op).map(|op| &op.kind) {
                    Some(OpKind::Restore(values)) => values
                        .get(&id_of_row)
                        .cloned()
                        .unwrap_or_else(|| Value::from("")),
                    _ => Value::from(columns.inserted_value(op, id_of_row).unwrap_or("")),
                },
                CellId::Appended(_) => Value::from(""),
            }
        })
        .collect()
}

/// A way to read the cells a column delete took, as they read before it
/// (ADR-0014 decision 3): the reading it was made in, and its edits then.
pub(crate) trait ColumnSource: Send + Sync {
    /// Cell `at` of each of logical rows `rows`, in order: an unedited
    /// field as its bytes where `parser` (the reading it goes back into)
    /// reads it the same (task 2.4c).
    fn values(
        &self,
        rows: &[Range<u32>],
        at: usize,
        parser: &RowParser,
    ) -> Result<Vec<Value>, ReadError>;
}

/// A row whose edits a column operation changed: its id, and its edits
/// before and after (`None`: none).
pub(crate) type Changed = (RowId, Option<Arc<RowEdits>>, Option<Arc<RowEdits>>);

/// What a column insert or delete command carries (task 2.4b,
/// `docs/tasks/2.4.md` §4): the operation and the rows whose edits it
/// changed, before and after, for an undo or redo by identity in the same
/// edits; and, for one in other edits (after a save, or in a replay), the
/// logical rows it applied to and, for a delete, a way to read the cells it
/// took. Opaque outside the core.
pub struct Column {
    pub(crate) base: BaseId,
    pub(crate) op: ColumnOp,
    /// The operations before it, and with it.
    pub(crate) before: Arc<Columns>,
    pub(crate) after: Arc<Columns>,
    /// Each row whose edits it changed: (row, edits before, edits after).
    pub(crate) rows: Vec<Changed>,
    /// The logical rows it applied to, as runs.
    pub(crate) applied: Vec<Range<u32>>,
    /// For a delete: the cells it took, read lazily.
    pub(crate) origin: Option<Arc<dyn ColumnSource>>,
    /// For cells put back by value: their values, in the rows' order.
    pub(crate) restored: Option<Arc<[Value]>>,
}

impl Column {
    /// The logical column.
    #[must_use]
    pub fn at(&self) -> usize {
        self.op.at
    }

    /// Whether the operation inserts a column (rather than deleting one).
    #[must_use]
    pub fn inserts(&self) -> bool {
        self.op.inserts()
    }

    /// How many rows it applied to.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.applied
            .iter()
            .map(|run| usize::try_from(run.end - run.start).unwrap_or(usize::MAX))
            .sum()
    }

    /// The cells it takes or puts back, by value: the inserted value in
    /// each row it applied to, or the deleted cells as they read then (an
    /// unedited field as its bytes where `parser` reads it the same).
    pub(crate) fn values(&self, parser: &RowParser) -> Result<Vec<Value>, ReadError> {
        if let Some(value) = self.op.value() {
            return Ok(vec![Value::Text(Arc::clone(value)); self.rows()]);
        }
        if let Some(values) = &self.restored {
            return Ok(values.iter().map(|value| value.fit(parser)).collect());
        }
        match &self.origin {
            Some(origin) => origin.values(&self.applied, self.op.at, parser),
            None => Ok(Vec::new()),
        }
    }
}

impl PartialEq for Column {
    fn eq(&self, other: &Column) -> bool {
        self.base == other.base
            && self.op == other.op
            && Arc::ptr_eq(&self.before, &other.before)
            && Arc::ptr_eq(&self.after, &other.after)
    }
}

impl Eq for Column {}

impl Hash for Column {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.hash(state);
        self.op.id.hash(state);
        self.op.at.hash(state);
    }
}

impl fmt::Debug for Column {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Column")
            .field("base", &self.base)
            .field("op", &self.op)
            .field("rows", &self.rows.len())
            .field("applied", &self.applied)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Rows {
    /// The same rows of the same file: the same command's, or one made by
    /// the same edit (the edits and inserted rows are the same values,
    /// shared).
    fn eq(&self, other: &Rows) -> bool {
        self.base == other.base
            && self.pieces == other.pieces
            && self.inserted.len() == other.inserted.len()
            && self
                .inserted
                .iter()
                .zip(&other.inserted)
                .all(|(a, b)| a.0 == b.0 && Arc::ptr_eq(&a.1, &b.1))
            && self.edits.len() == other.edits.len()
            && self
                .edits
                .iter()
                .zip(&other.edits)
                .all(|(a, b)| a.0 == b.0 && Arc::ptr_eq(&a.1, &b.1))
    }
}

impl Eq for Rows {}

impl Hash for Rows {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.base.hash(state);
        self.count.hash(state);
        for ids in self.ids() {
            ids.hash(state);
        }
    }
}

impl fmt::Debug for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rows")
            .field("base", &self.base)
            .field("pieces", &self.pieces)
            .field("edited", &self.edits.len())
            .finish_non_exhaustive()
    }
}
