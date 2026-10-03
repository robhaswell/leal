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

use super::overlay::{InsertedRow, Overlay, RowEdits};
use super::rows::{Piece, RowId};
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
    /// cells' values: fewer if the file doesn't have them all.
    fn values(&self, rows: Range<u32>, edits: &Overlay) -> Result<Vec<Vec<String>>, ReadError>;
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
    /// read from, with its edits. Original rows are read now (ADR-0014
    /// decision 3). Fewer than [`len`](Self::len) if some can't be had.
    pub(crate) fn values(&self) -> Result<Vec<Vec<String>>, ReadError> {
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
                    );
                    if let Some(origin) = &self.origin {
                        values.extend(origin.values(start..start + len, &originals)?);
                    }
                }
                Piece::Inserted { first, len, .. } => {
                    for n in first..first + len {
                        if let Some(row) = inserted.get(&n) {
                            let edits = edits.get(&RowId::inserted(n)).map(|e| e.as_ref());
                            values.push(inserted_values(row, edits));
                        }
                    }
                }
            }
        }
        Ok(values)
    }
}

/// An inserted row's cells as it reads with `edits`.
pub(crate) fn inserted_values(row: &InsertedRow, edits: Option<&RowEdits>) -> Vec<String> {
    let fields = row.fields();
    let len = edits.map_or(fields.len(), |edits| fields.len().max(edits.end()));
    (0..len)
        .map(|column| {
            edits
                .and_then(|edits| edits.get(column))
                .or_else(|| fields.get(column).map(AsRef::as_ref))
                .unwrap_or("")
                .to_owned()
        })
        .collect()
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
