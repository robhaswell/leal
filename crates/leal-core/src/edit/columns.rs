//! Column inserts and deletes (task 2.4b, `docs/tasks/2.4.md` §3): the
//! operations, the identities of a row's cells, and how a row's cells are
//! laid out once columns have been inserted or deleted.
//!
//! **Cell identity.** Every cell of a row has a [`CellId`] for life: one
//! of its own fields ([`CellId::Field`]), the cell a column insert gave it
//! ([`CellId::Inserted`]), or a cell past its own fields, a hatched cell
//! ([`CellId::Appended`]). Edits are kept by identity, so a column insert
//! or delete moves every cell without rewriting any edit, and a deleted
//! column's edits stay, hidden, for its undo.
//!
//! **A row's layout** is which cell each logical column is. For almost
//! every row it is the *default layout*: the row's own fields with each
//! operation applied in turn where oracle rule 6 says it applies (ADR-0004
//! decision 5: an insert at `c` needs at least `c` cells, a delete at `c`
//! more than `c`, and a blank line never changes), then its hatched cells,
//! which no operation reaches. That depends only on the row's own field
//! count, so a table per field count serves every unedited row
//! ([`Columns::fold_len`], [`Columns::fold`]), and an operation costs
//! nothing per unedited row.
//!
//! An edited row may differ: its hatched cells count towards its length,
//! and once edited a blank line isn't one. When an operation decides
//! otherwise for an edited row than the default would, the row's layout is
//! written out ([`RowEdits`](super::RowEdits)' explicit layout), and later
//! operations change it directly. So an operation costs O(edited rows) (a
//! look at each), never O(rows). This replaces the design's per-operation
//! exception lists: the same information, kept with the row.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use super::value::{RawField, Value};
use super::{RowEdits, RowId};

/// A column operation's id, unique within one store's edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct OpId(pub(crate) u32);

/// Which cell of its row a cell is, for life (see the module's docs).
/// Ordered fields first, then inserted cells, then hatched ones, so a
/// row's edits sort that way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum CellId {
    /// The row's own field `k` (an inserted row's own value `k`).
    Field(u32),
    /// The cell column insert `op` gave the row.
    Inserted(OpId),
    /// A cell past the row's own fields (a hatched cell, ADR-0005
    /// decision 2): position `j` of the row before any column operation
    /// reached it.
    Appended(u32),
}

impl CellId {
    /// The id of position `p` of a row with `fields` own fields, before any
    /// column operation: a field, or a hatched cell past them.
    pub(crate) fn base(p: usize, fields: usize) -> CellId {
        let n = u32::try_from(p).unwrap_or(u32::MAX);
        if p < fields {
            CellId::Field(n)
        } else {
            CellId::Appended(n)
        }
    }
}

/// What a column operation does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OpKind {
    /// Inserts a cell holding this value.
    Insert(Arc<str>),
    /// Inserts a cell holding each row's own value: a column delete's
    /// cells put back by value, by its undo after a save or in a replay
    /// (ADR-0014 decision 3).
    Restore(Arc<BTreeMap<RowId, Value>>),
    /// Deletes a cell.
    Delete,
}

/// A column insert or delete at logical column `at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ColumnOp {
    pub(crate) id: OpId,
    pub(crate) at: usize,
    pub(crate) kind: OpKind,
    /// Inserted rows numbered from this on were inserted after the
    /// operation, already laid out as the document was then: it never
    /// applies to them.
    pub(crate) inserted_before: u32,
}

impl ColumnOp {
    /// Whether it applies to a row of `len` cells (oracle rule 6, ADR-0004
    /// decision 5), `blank` if the row is a blank line of the file,
    /// unedited. A row that edits and deletes have left with no cells is a
    /// blank line too, which an insert skips (ADR-0014 decision 6); a
    /// restore, which is only ever by value, puts back what a delete took
    /// from it, so its rule takes it in (the rows it differs for are given
    /// layouts of their own anyway).
    pub(crate) fn applies(&self, len: usize, blank: bool) -> bool {
        !blank
            && match self.kind {
                OpKind::Insert(_) => len > 0 && len >= self.at,
                OpKind::Restore(_) => len >= self.at,
                OpKind::Delete => len > self.at,
            }
    }

    /// Whether it can apply to inserted row `n` at all.
    fn reaches(&self, inserted: Option<u32>) -> bool {
        inserted.is_none_or(|n| n < self.inserted_before)
    }

    /// The length of a row of `len` cells after it, where it applies.
    fn len_after(&self, len: usize) -> usize {
        match self.kind {
            OpKind::Insert(_) | OpKind::Restore(_) => len + 1,
            OpKind::Delete => len - 1,
        }
    }

    /// Applies it to an explicit layout, which it applies to: the cell it
    /// deleted, for a delete.
    pub(crate) fn apply(&self, layout: &mut Vec<CellId>) -> Option<CellId> {
        match self.kind {
            OpKind::Insert(_) | OpKind::Restore(_) => {
                layout.insert(self.at.min(layout.len()), CellId::Inserted(self.id));
                None
            }
            OpKind::Delete => (self.at < layout.len()).then(|| layout.remove(self.at)),
        }
    }

    /// The inserted value, for an insert of one value.
    pub(crate) fn value(&self) -> Option<&Arc<str>> {
        match &self.kind {
            OpKind::Insert(value) => Some(value),
            OpKind::Restore(_) | OpKind::Delete => None,
        }
    }

    /// Whether it inserts a cell.
    pub(crate) fn inserts(&self) -> bool {
        !matches!(self.kind, OpKind::Delete)
    }

    /// The value it gives row `row`, for an insert, as it reads.
    pub(crate) fn value_for(&self, row: RowId) -> Option<&str> {
        match &self.kind {
            OpKind::Insert(value) => Some(value),
            OpKind::Restore(values) => Some(values.get(&row).map_or("", Value::text)),
            OpKind::Delete => None,
        }
    }

    /// The field it puts back in row `row` as its bytes, if it does.
    pub(crate) fn raw_for(&self, row: RowId) -> Option<&RawField> {
        match &self.kind {
            OpKind::Restore(values) => values.get(&row).and_then(Value::raw),
            OpKind::Insert(_) | OpKind::Delete => None,
        }
    }

    /// Every value it inserts, as it reads.
    pub(crate) fn values(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match &self.kind {
            OpKind::Insert(value) => Box::new(std::iter::once(value.as_ref())),
            OpKind::Restore(values) => Box::new(values.values().map(Value::text)),
            OpKind::Delete => Box::new(std::iter::empty()),
        }
    }

    /// Every value it inserts as text, which a save encodes: not the fields
    /// it puts back as their bytes.
    pub(crate) fn texts(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match &self.kind {
            OpKind::Restore(values) => Box::new(
                values
                    .values()
                    .filter(|value| value.raw().is_none())
                    .map(Value::text),
            ),
            OpKind::Insert(_) | OpKind::Delete => self.values(),
        }
    }
}

/// What the default layout needs to know about a row: how many cells of
/// its own it has, whether it is a blank line of the file, and, for an
/// inserted row, its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Own {
    pub(crate) fields: usize,
    pub(crate) blank: bool,
    pub(crate) inserted: Option<u32>,
}

impl Own {
    /// A row of the file with `fields` fields.
    pub(crate) fn original(fields: usize, blank: bool) -> Own {
        Own {
            fields,
            blank,
            inserted: None,
        }
    }

    /// Inserted row `n`, with `fields` values.
    pub(crate) fn inserted(n: u32, fields: usize) -> Own {
        Own {
            fields,
            blank: false,
            inserted: Some(n),
        }
    }
}

/// The field counts below this have their default layout worked out once
/// per list of operations; the marks keep field counts up to 126 exactly
/// (`diagnostics::marks::WIDE`).
pub(crate) const TABLE: usize = 128;

/// The column operations in effect, in order, and the default layout of
/// each field count they give (see the module's docs). Empty, it is the
/// identity and costs the readers a check.
#[derive(Clone, Debug, Default)]
pub(crate) struct Columns {
    ops: Vec<ColumnOp>,
    /// `folds[n]`: the default layout of a non-blank row of the file with
    /// `n` fields (`n` < [`TABLE`]). Empty with no operations.
    folds: Vec<Arc<[CellId]>>,
}

impl PartialEq for Columns {
    fn eq(&self, other: &Columns) -> bool {
        self.ops == other.ops
    }
}

impl Eq for Columns {}

impl Columns {
    /// No operations.
    pub(crate) const NONE: Columns = Columns {
        ops: Vec::new(),
        folds: Vec::new(),
    };

    /// No operations: the identity.
    pub(crate) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The operations, in order.
    pub(crate) fn ops(&self) -> &[ColumnOp] {
        &self.ops
    }

    /// The operation with `id`, if it is in effect.
    pub(crate) fn op(&self, id: OpId) -> Option<&ColumnOp> {
        self.ops.iter().find(|op| op.id == id)
    }

    /// These operations and then `op`.
    pub(crate) fn with(&self, op: ColumnOp) -> Columns {
        let folds = (0..TABLE)
            .map(|n| {
                let mut layout: Vec<CellId> = match self.folds.get(n) {
                    Some(fold) => fold.to_vec(),
                    None => (0..n).map(|k| CellId::base(k, n)).collect(),
                };
                if op.applies(layout.len(), false) {
                    op.apply(&mut layout);
                }
                Arc::from(layout)
            })
            .collect();
        let mut ops = self.ops.clone();
        ops.push(op);
        Columns { ops, folds }
    }

    /// How many cells a row's own cells (and the inserted ones it has
    /// gained) make in its default layout.
    pub(crate) fn fold_len(&self, own: Own) -> usize {
        if self.ops.is_empty() || own.blank {
            return own.fields;
        }
        if own.inserted.is_none()
            && let Some(fold) = self.folds.get(own.fields)
        {
            return fold.len();
        }
        let mut len = own.fields;
        for op in &self.ops {
            if op.reaches(own.inserted) && op.applies(len, false) {
                len = op.len_after(len);
            }
        }
        len
    }

    /// The length of the default layout of a non-blank row of the file
    /// with `fields` fields: the most common field count's is the column
    /// count.
    pub(crate) fn fold_fields(&self, fields: usize) -> usize {
        self.fold_len(Own::original(fields, false))
    }

    /// A row's default layout, its own cells and inserted ones (not its
    /// hatched cells, which come after).
    pub(crate) fn fold(&self, own: Own) -> Fold<'_> {
        if self.ops.is_empty() || own.blank {
            return Fold::identity(own.fields);
        }
        if own.inserted.is_none()
            && let Some(fold) = self.folds.get(own.fields)
        {
            return Fold::Table(fold);
        }
        let mut layout: Vec<CellId> = (0..own.fields)
            .map(|k| CellId::base(k, own.fields))
            .collect();
        for op in &self.ops {
            if op.reaches(own.inserted) && op.applies(layout.len(), false) {
                op.apply(&mut layout);
            }
        }
        Fold::Built(layout)
    }

    /// The value inserted cell `id` of row `row` holds, if its operation
    /// is in effect.
    pub(crate) fn inserted_value(&self, id: OpId, row: RowId) -> Option<&str> {
        self.op(id).and_then(|op| op.value_for(row))
    }

    /// The field inserted cell `id` of row `row` holds as its bytes, if
    /// its operation is in effect and put it back so (task 2.4c).
    pub(crate) fn inserted_raw(&self, id: OpId, row: RowId) -> Option<&RawField> {
        self.op(id).and_then(|op| op.raw_for(row))
    }
}

/// A default layout ([`Columns::fold`]).
#[derive(Clone, Debug)]
pub(crate) enum Fold<'a> {
    /// No operation reaches the row: `len` cells in order, the first
    /// `fields` its own and the rest hatched.
    Identity { len: usize, fields: usize },
    /// From the table.
    Table(&'a [CellId]),
    /// Worked out for this row.
    Built(Vec<CellId>),
}

impl Fold<'_> {
    /// A row's own `fields` cells, in order.
    pub(crate) fn identity(fields: usize) -> Fold<'static> {
        Fold::Identity {
            len: fields,
            fields,
        }
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Fold::Identity { len, .. } => *len,
            Fold::Table(ids) => ids.len(),
            Fold::Built(ids) => ids.len(),
        }
    }

    /// Cell `c`'s id, if the layout has one.
    pub(crate) fn get(&self, c: usize) -> Option<CellId> {
        match self {
            Fold::Identity { len, fields } => (c < *len).then(|| CellId::base(c, *fields)),
            Fold::Table(ids) => ids.get(c).copied(),
            Fold::Built(ids) => ids.get(c).copied(),
        }
    }

    /// The ids, written out.
    pub(crate) fn as_slice(&self) -> Cow<'_, [CellId]> {
        match self {
            Fold::Identity { len, fields } => (0..*len).map(|k| CellId::base(k, *fields)).collect(),
            Fold::Table(ids) => Cow::Borrowed(ids),
            Fold::Built(ids) => Cow::Borrowed(ids),
        }
    }
}

/// The parts of a row's edits a layout is made from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Parts<'a> {
    /// The row's own field count when it was first edited.
    pub(crate) fields: usize,
    /// It is a blank line of the file.
    pub(crate) blank: bool,
    /// The edited cells, sorted by id.
    pub(crate) cells: &'a [(CellId, Arc<str>)],
    /// Its explicit layout, if it has one.
    pub(crate) layout: Option<&'a [CellId]>,
}

/// One past the last base position edited (a field's or a hatched
/// cell's): with no column operation, the row is at least this long.
pub(crate) fn base_end(cells: &[(CellId, Arc<str>)]) -> usize {
    cells
        .iter()
        .filter_map(|&(id, _)| match id {
            CellId::Field(k) | CellId::Appended(k) => Some(to_usize(k) + 1),
            CellId::Inserted(_) => None,
        })
        .max()
        .unwrap_or(0)
}

/// One past the last hatched cell edited, or `fields`: where a default
/// layout's hatched cells end.
pub(crate) fn appended_end(fields: usize, cells: &[(CellId, Arc<str>)]) -> usize {
    match cells.last() {
        Some(&(CellId::Appended(j), _)) => fields.max(to_usize(j) + 1),
        _ => fields,
    }
}

/// An id for a new hatched cell of a row whose layout is written out:
/// past every hatched cell it has, shown or hidden, and its own fields.
pub(crate) fn fresh_appended(fields: usize, ids: &[CellId], cells: &[(CellId, Arc<str>)]) -> u32 {
    let past = |id: &CellId| match *id {
        CellId::Appended(j) => to_usize(j) + 1,
        _ => 0,
    };
    let next = ids
        .iter()
        .chain(cells.iter().map(|(id, _)| id))
        .map(past)
        .max()
        .unwrap_or(0)
        .max(fields);
    u32::try_from(next).unwrap_or(u32::MAX)
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Which cell each logical column of a row is: its default layout, or the
/// one its edits keep ([`RowEdits::layout`]).
#[derive(Clone, Debug)]
pub(crate) enum Layout<'a> {
    /// The default layout of the row's own cells, then `tail` hatched
    /// cells, `Appended(fields..fields + tail)`, which no operation reaches.
    Default {
        fold: Fold<'a>,
        fields: usize,
        tail: usize,
    },
    /// Written out.
    Explicit(&'a [CellId]),
}

impl<'a> Layout<'a> {
    /// The layout of a row with `own` cells as read and `edits`, under
    /// `columns`. With no column operations it is task 2.1's: as long as
    /// the row as read, or its last edited cell, the cells numbered as its
    /// edits were made (their field count, which is the row's as read
    /// unless the first 64 KB turned out stale, task 1.9).
    pub(crate) fn of(columns: &'a Columns, own: Own, edits: Option<&'a RowEdits>) -> Layout<'a> {
        Layout::of_parts(columns, own, edits.map(RowEdits::parts))
    }

    /// [`of`](Self::of), from the parts of a row's edits.
    pub(crate) fn of_parts(columns: &'a Columns, own: Own, edits: Option<Parts<'a>>) -> Layout<'a> {
        let Some(edits) = edits else {
            return Layout::Default {
                fold: columns.fold(own),
                fields: own.fields,
                tail: 0,
            };
        };
        if let Some(ids) = edits.layout {
            return Layout::Explicit(ids);
        }
        if columns.is_empty() {
            let len = own.fields.max(base_end(edits.cells));
            return Layout::Default {
                fold: Fold::Identity {
                    len,
                    fields: edits.fields,
                },
                fields: edits.fields,
                tail: 0,
            };
        }
        let own = Own {
            fields: edits.fields,
            blank: edits.blank,
            inserted: own.inserted,
        };
        Layout::Default {
            fold: columns.fold(own),
            fields: edits.fields,
            tail: appended_end(edits.fields, edits.cells) - edits.fields,
        }
    }

    /// The id a hatched edit at logical column `c`, past the row's end,
    /// takes in a default layout: past every cell, so no operation reaches
    /// it (an explicit layout makes up its own, [`fresh_appended`]). With
    /// no operation it is the id [`get`](Self::get) reads there once the
    /// edit is made: a field's, if the row was edited from a stale first
    /// 64 KB (task 1.9) with more fields than it is read with now.
    pub(crate) fn hatched(&self, c: usize) -> Option<CellId> {
        match self {
            Layout::Default {
                fold: Fold::Identity { .. },
                fields,
                ..
            } => Some(CellId::base(c, *fields)),
            Layout::Default { fold, fields, .. } => {
                let j = c.checked_sub(fold.len())? + fields;
                Some(CellId::base(j, 0))
            }
            Layout::Explicit(_) => None,
        }
    }

    /// How many cells the row has.
    pub(crate) fn len(&self) -> usize {
        match self {
            Layout::Default { fold, tail, .. } => fold.len() + tail,
            Layout::Explicit(ids) => ids.len(),
        }
    }

    /// Cell `c`'s id, or `None` past the row's end.
    pub(crate) fn get(&self, c: usize) -> Option<CellId> {
        match self {
            Layout::Default { fold, fields, tail } => {
                let folded = fold.len();
                if c < folded {
                    fold.get(c)
                } else {
                    let j = fields + (c - folded);
                    (c - folded < *tail)
                        .then(|| CellId::Appended(u32::try_from(j).unwrap_or(u32::MAX)))
                }
            }
            Layout::Explicit(ids) => ids.get(c).copied(),
        }
    }

    /// Where hatched cell `j` is, in a default layout's tail.
    pub(crate) fn tail_position(&self, j: usize) -> Option<usize> {
        match self {
            Layout::Default { fold, fields, tail } => {
                let k = j.checked_sub(*fields)?;
                (k < *tail).then(|| fold.len() + k)
            }
            Layout::Explicit(_) => None,
        }
    }

    /// Every id, in order, written out (a hatched edit far right makes a
    /// long list: callers that only want the cells with something in them
    /// use the fold and the edits instead).
    pub(crate) fn ids(&self) -> Cow<'_, [CellId]> {
        match self {
            Layout::Default { fold, tail: 0, .. } => fold.as_slice(),
            Layout::Default { .. } => (0..self.len()).filter_map(|c| self.get(c)).collect(),
            Layout::Explicit(ids) => Cow::Borrowed(ids),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(id: u32, at: usize, before: u32) -> ColumnOp {
        ColumnOp {
            id: OpId(id),
            at,
            kind: OpKind::Insert(Arc::from("v")),
            inserted_before: before,
        }
    }

    fn delete(id: u32, at: usize) -> ColumnOp {
        ColumnOp {
            id: OpId(id),
            at,
            kind: OpKind::Delete,
            inserted_before: 0,
        }
    }

    /// The default layout, the slow way: each operation on a vector.
    fn model(ops: &[ColumnOp], own: Own) -> Vec<CellId> {
        let mut layout: Vec<CellId> = (0..own.fields)
            .map(|k| CellId::base(k, own.fields))
            .collect();
        for op in ops {
            if op.reaches(own.inserted) && op.applies(layout.len(), own.blank) {
                op.apply(&mut layout);
            }
        }
        layout
    }

    #[test]
    fn the_table_and_the_walk_agree_with_the_model() {
        let ops = [
            insert(0, 2, 0),
            delete(1, 0),
            insert(2, 5, 3),
            delete(3, 4),
            insert(4, 0, 9),
        ];
        let mut columns = Columns::default();
        for op in ops.iter().cloned() {
            columns = columns.with(op);
        }
        for fields in [0, 1, 2, 3, 4, 5, 6, 9, 130, 200] {
            for own in [
                Own::original(fields, false),
                Own::inserted(1, fields),
                Own::inserted(5, fields),
            ] {
                let expected = model(&ops, own);
                assert_eq!(columns.fold(own).as_slice().as_ref(), expected, "{own:?}");
                assert_eq!(columns.fold_len(own), expected.len(), "{own:?}");
            }
        }
        // A blank line never changes.
        let blank = Own::original(1, true);
        assert_eq!(columns.fold_len(blank), 1);
        let row = RowId::original(0);
        assert_eq!(columns.inserted_value(OpId(2), row), Some("v"));
        assert!(columns.inserted_value(OpId(1), row).is_none());
    }

    #[test]
    fn rule_6_insert_needs_the_position_and_delete_a_cell_there() {
        let op = insert(0, 3, 0);
        assert!(op.applies(3, false));
        assert!(!op.applies(2, false));
        assert!(!op.applies(5, true), "never a blank line");
        let op = insert(0, 0, 0);
        assert!(op.applies(1, false));
        assert!(!op.applies(0, false), "a row with no cells is a blank line");
        let op = delete(0, 3);
        assert!(op.applies(4, false));
        assert!(!op.applies(3, false));
    }

    /// A hatched edit with no column operation takes the id `get` reads
    /// back at its column, also when the row was edited with more fields
    /// (a stale first 64 KB, task 1.9) than it is read with now.
    #[test]
    fn a_hatched_edit_reads_back_where_it_was_made() {
        let none = Columns::default();
        let cells: Vec<(CellId, Arc<str>)> = vec![(CellId::Field(0), Arc::from("e"))];
        let parts = |cells| Parts {
            fields: 4,
            blank: false,
            cells,
            layout: None,
        };
        let layout = Layout::of_parts(&none, Own::original(2, false), Some(parts(&cells)));
        assert_eq!(layout.get(3), None);
        let id = layout.hatched(3).unwrap();
        let mut edited = cells.clone();
        edited.push((id, Arc::from("v")));
        edited.sort_unstable_by_key(|&(id, _)| id);
        let after = Layout::of_parts(&none, Own::original(2, false), Some(parts(&edited)));
        assert_eq!(after.get(3), Some(id));
        // Read with as many fields as it was edited with, it is hatched.
        let layout = Layout::of_parts(&none, Own::original(4, false), Some(parts(&cells)));
        assert_eq!(layout.hatched(5), Some(CellId::Appended(5)));
    }

    #[test]
    fn ids_sort_fields_inserted_then_hatched() {
        let mut ids = vec![
            CellId::Appended(3),
            CellId::Inserted(OpId(0)),
            CellId::Field(2),
            CellId::Field(0),
        ];
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                CellId::Field(0),
                CellId::Field(2),
                CellId::Inserted(OpId(0)),
                CellId::Appended(3)
            ]
        );
        assert_eq!(CellId::base(1, 2), CellId::Field(1));
        assert_eq!(CellId::base(2, 2), CellId::Appended(2));
    }
}
