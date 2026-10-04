//! The overlay: what the commands changed, by row id, the inserted rows
//! and the piece list, and the store that shares it between a document's
//! readings and its jobs.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::Lineage;
use super::columns::{CellId, Columns, Fold, Layout, Own, Parts};
#[cfg(test)]
use super::rows::Piece;
use super::rows::{RowId, RowMap};
use super::structural::BaseId;
use super::value::Value;
use crate::diagnostics::DiagnosticKind;

/// The field-level diagnostics a row or a cell has (task 1.7's flagged
/// kinds), one bit each, for its gutter marker and each kind's
/// **Previous** and **Next**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Kinds(u8);

impl Kinds {
    pub(crate) const UNTERMINATED_QUOTE: Kinds = Kinds(1);
    pub(crate) const TEXT_AFTER_CLOSING_QUOTE: Kinds = Kinds(2);
    pub(crate) const INVALID_ENCODING: Kinds = Kinds(4);
    pub(crate) const NUL_BYTES: Kinds = Kinds(8);

    /// The field-level kinds, and their bits.
    pub(crate) const FIELD_KINDS: [(DiagnosticKind, Kinds); 4] = [
        (DiagnosticKind::UnterminatedQuote, Kinds::UNTERMINATED_QUOTE),
        (
            DiagnosticKind::TextAfterClosingQuote,
            Kinds::TEXT_AFTER_CLOSING_QUOTE,
        ),
        (DiagnosticKind::InvalidEncoding, Kinds::INVALID_ENCODING),
        (DiagnosticKind::NulBytes, Kinds::NUL_BYTES),
    ];

    /// `kind`'s bit, or none for a kind that isn't field-level.
    pub(crate) fn of(kind: DiagnosticKind) -> Kinds {
        Kinds::FIELD_KINDS
            .iter()
            .find(|&&(k, _)| k == kind)
            .map_or(Kinds::default(), |&(_, bit)| bit)
    }

    pub(crate) fn insert(&mut self, other: Kinds) {
        self.0 |= other.0;
    }

    pub(crate) fn contains(self, other: Kinds) -> bool {
        other.0 != 0 && self.0 & other.0 == other.0
    }

    pub(crate) fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// One edited row: its new cell values, by [`CellId`], how its cells are
/// laid out if not by default, and what its diagnostics are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowEdits {
    /// How many fields the row has in the file (as it was read when the
    /// row was first edited), or an inserted row's values.
    fields: usize,
    /// The row is a blank line of the file: its default layout never
    /// changes (ADR-0004 decision 5).
    blank: bool,
    /// The edited cells, sorted by id: new values for some of the row's
    /// cells, which may be hidden (a deleted column's, kept for its undo).
    /// Empty only with an explicit layout.
    cells: Vec<(CellId, Arc<str>)>,
    /// The row's layout, if a column operation decided otherwise for it
    /// than its default layout would (`edit::columns`).
    layout: Option<Arc<[CellId]>>,
    /// The row's own fields that have field-level diagnostics, worked out
    /// from their bytes once, when the row was first edited, and shared by
    /// every later version of its edits: so an edit costs what its cells
    /// do, not the whole row (usually empty).
    flagged: Arc<[(usize, Kinds)]>,
    /// The field-level diagnostics of the row as it reads with no column
    /// operation: each unedited field's from `flagged`, each edited cell
    /// on its new value (ADR-0008 decision 2). With column operations,
    /// [`kinds_in`](Self::kinds_in) works them out.
    kinds: Kinds,
    /// A hash of the row's bytes, if it was read from the first 64 KB kept
    /// in memory: if those turn out to be a different version of the file
    /// (task 1.9), the edit is checked against the row in the trusted copy
    /// (`Document::edit_conflicts`).
    head_hash: Option<u64>,
}

impl RowEdits {
    /// A row of `fields` fields in the file, with `cells` edited (sorted by
    /// id) and its own fields' diagnostics `flagged`, laid out by default.
    pub(crate) fn new(
        fields: usize,
        cells: Vec<(CellId, Arc<str>)>,
        flagged: Arc<[(usize, Kinds)]>,
        head_hash: Option<u64>,
    ) -> RowEdits {
        debug_assert!(cells.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let mut kinds = Kinds::default();
        for &(field, bits) in flagged.iter() {
            if cells
                .binary_search_by_key(&field_id(field), |&(id, _)| id)
                .is_err()
            {
                kinds.insert(bits);
            }
        }
        if cells.iter().any(|(_, value)| value.contains('\0')) {
            kinds.insert(Kinds::NUL_BYTES);
        }
        RowEdits {
            fields,
            blank: false,
            cells,
            layout: None,
            flagged,
            kinds,
            head_hash,
        }
    }

    /// The same, of a blank line of the file.
    #[must_use]
    pub(crate) fn blank(mut self, blank: bool) -> RowEdits {
        self.blank = blank;
        self
    }

    /// The same, laid out as `layout` says (`None`: by default).
    #[must_use]
    pub(crate) fn with_layout(mut self, layout: Option<Arc<[CellId]>>) -> RowEdits {
        self.layout = layout;
        self
    }

    /// How many fields of its own the row had when it was first edited.
    pub(crate) fn fields(&self) -> usize {
        self.fields
    }

    /// Whether the row is a blank line of the file.
    pub(crate) fn is_blank(&self) -> bool {
        self.blank
    }

    /// The explicit layout, if it has one.
    pub(crate) fn layout(&self) -> Option<&[CellId]> {
        self.layout.as_deref()
    }

    /// The parts a layout is made from.
    pub(crate) fn parts(&self) -> Parts<'_> {
        Parts {
            fields: self.fields,
            blank: self.blank,
            cells: &self.cells,
            layout: self.layout.as_deref(),
        }
    }

    /// The new value of cell `id`, if it is edited.
    pub(crate) fn get(&self, id: CellId) -> Option<&str> {
        let at = self.cells.binary_search_by_key(&id, |&(c, _)| c).ok()?;
        Some(&self.cells[at].1)
    }

    /// The edited cells, sorted by id.
    pub(crate) fn cells(&self) -> &[(CellId, Arc<str>)] {
        &self.cells
    }

    pub(crate) fn flagged(&self) -> &Arc<[(usize, Kinds)]> {
        &self.flagged
    }

    pub(crate) fn head_hash(&self) -> Option<u64> {
        self.head_hash
    }

    /// What the default layout needs to know of the row, inserted row `n`
    /// if `inserted`.
    pub(crate) fn own(&self, inserted: Option<u32>) -> Own {
        Own {
            fields: self.fields,
            blank: self.blank,
            inserted,
        }
    }

    /// The row's layout under `columns` (inserted row `n` if `inserted`),
    /// as its edits were made.
    pub(crate) fn layout_in<'a>(
        &'a self,
        columns: &'a Columns,
        inserted: Option<u32>,
    ) -> Layout<'a> {
        Layout::of(columns, self.own(inserted), Some(self))
    }

    /// How many cells the row has under `columns`, as its edits were made
    /// (the marks use it; readers count the row's own fields as they read
    /// them, `RowView::len`).
    pub(crate) fn len_in(&self, columns: &Columns, inserted: Option<u32>) -> usize {
        self.layout_in(columns, inserted).len()
    }

    /// Row `row`'s field-level diagnostics under `columns`, these being its
    /// edits: each of its own fields still there and unedited by
    /// `flagged`, each edited cell still there, and each inserted cell, by
    /// its value (only a NUL can be in one).
    pub(crate) fn kinds_in(&self, columns: &Columns, row: RowId) -> Kinds {
        if columns.is_empty() && self.layout.is_none() {
            return self.kinds;
        }
        let layout = self.layout_in(columns, row.inserted_index());
        let mut kinds = Kinds::default();
        let mut add = |id: CellId| match self.get(id) {
            Some(value) => {
                if value.contains('\0') {
                    kinds.insert(Kinds::NUL_BYTES);
                }
            }
            None => match id {
                CellId::Field(k) => {
                    let k = to_usize(k);
                    if let Ok(i) = self.flagged.binary_search_by_key(&k, |&(f, _)| f) {
                        kinds.insert(self.flagged[i].1);
                    }
                }
                CellId::Inserted(op) => match columns.inserted_raw(op, row) {
                    Some(raw) => kinds.insert(raw.kinds()),
                    None => {
                        if columns
                            .inserted_value(op, row)
                            .is_some_and(|v| v.contains('\0'))
                        {
                            kinds.insert(Kinds::NUL_BYTES);
                        }
                    }
                },
                CellId::Appended(_) => {}
            },
        };
        match &layout {
            // Only the hatched cells edited can hold anything.
            Layout::Default { fold, .. } => {
                for c in 0..fold.len() {
                    if let Some(id) = fold.get(c) {
                        add(id);
                    }
                }
                for &(id, _) in &self.cells {
                    if matches!(id, CellId::Appended(_)) {
                        add(id);
                    }
                }
            }
            Layout::Explicit(ids) => ids.iter().for_each(|&id| add(id)),
        }
        kinds
    }

    /// Whether the row is a blank line under `columns`: a blank line of the
    /// file whose one cell is its own field, unedited (a column delete can
    /// make an edited blank line one again).
    pub(crate) fn is_blank_in(&self, columns: &Columns, inserted: Option<u32>) -> bool {
        let field = CellId::Field(0);
        self.blank && self.get(field).is_none() && {
            let layout = self.layout_in(columns, inserted);
            layout.len() == 1 && layout.get(0) == Some(field)
        }
    }

    /// The edited cells the row shows under `columns` (inserted row `n` if
    /// `inserted`), with their logical columns, in column order: not the
    /// hidden ones a column delete took.
    pub(crate) fn shown(
        &self,
        columns: &Columns,
        inserted: Option<u32>,
    ) -> Vec<(usize, &Arc<str>)> {
        let layout = self.layout_in(columns, inserted);
        let mut shown: Vec<(usize, &Arc<str>)> = match &layout {
            Layout::Default {
                fold: Fold::Identity { .. },
                ..
            } => self
                .cells
                .iter()
                .filter_map(|(id, value)| match *id {
                    CellId::Field(k) | CellId::Appended(k) => Some((to_usize(k), value)),
                    CellId::Inserted(_) => None,
                })
                .collect(),
            Layout::Default { fold, .. } => {
                let folded = fold.as_slice();
                self.cells
                    .iter()
                    .filter_map(|(id, value)| {
                        let column = match *id {
                            CellId::Appended(j) => layout.tail_position(to_usize(j)),
                            id => folded.iter().position(|&f| f == id),
                        };
                        Some((column?, value))
                    })
                    .collect()
            }
            Layout::Explicit(ids) => ids
                .iter()
                .enumerate()
                .filter_map(|(c, &id)| {
                    let at = self.cells.binary_search_by_key(&id, |&(c, _)| c).ok()?;
                    Some((c, &self.cells[at].1))
                })
                .collect(),
        };
        shown.sort_unstable_by_key(|&(column, _)| column);
        shown
    }

    /// How many of the edited cells the row shows under `columns`
    /// (inserted row `n` if `inserted`): [`shown`](Self::shown)'s length,
    /// without making the list.
    pub(crate) fn shown_len(&self, columns: &Columns, inserted: Option<u32>) -> usize {
        let is_shown = |id: CellId, layout: &Layout<'_>| match (layout, id) {
            (
                Layout::Default {
                    fold: Fold::Identity { .. },
                    ..
                },
                id,
            ) => !matches!(id, CellId::Inserted(_)),
            (Layout::Default { .. }, CellId::Appended(j)) => {
                layout.tail_position(to_usize(j)).is_some()
            }
            (Layout::Default { fold, .. }, id) => (0..fold.len()).any(|c| fold.get(c) == Some(id)),
            (Layout::Explicit(ids), id) => ids.contains(&id),
        };
        if columns.is_empty() && self.layout.is_none() {
            return self.cells.len();
        }
        let layout = self.layout_in(columns, inserted);
        self.cells
            .iter()
            .filter(|&&(id, _)| is_shown(id, &layout))
            .count()
    }

    /// Whether these edits read the same as `other`'s under `columns`: the
    /// same cells in the same places, with the same values, hidden ones
    /// included, though hatched cells may have other ids (an undo by value
    /// gives new ones).
    pub(crate) fn reads_as(
        &self,
        other: &RowEdits,
        columns: &Columns,
        inserted: Option<u32>,
    ) -> bool {
        if self == other {
            return true;
        }
        let (a, b) = (
            self.layout_in(columns, inserted),
            other.layout_in(columns, inserted),
        );
        let (a, b) = (a.ids(), b.ids());
        let same_cell = |x: CellId, y: CellId| match (x, y) {
            (CellId::Appended(_), CellId::Appended(_)) => self.get(x) == other.get(y),
            _ => x == y && self.get(x) == other.get(y),
        };
        let hidden = |edits: &RowEdits, shown: &[CellId]| -> Vec<(CellId, Arc<str>)> {
            edits
                .cells
                .iter()
                .filter(|(id, _)| !shown.contains(id))
                .cloned()
                .collect()
        };
        a.len() == b.len()
            && a.iter().zip(b.iter()).all(|(&x, &y)| same_cell(x, y))
            && hidden(self, &a) == hidden(other, &b)
    }
}

/// Field `k`'s id.
fn field_id(k: usize) -> CellId {
    CellId::Field(u32::try_from(k).unwrap_or(u32::MAX))
}

/// An inserted row (task 2.4a): its own values, and the gap it was
/// inserted at, for life. Its cell edits are [`RowEdits`] like any row's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InsertedRow {
    /// The physical row it was inserted before (the file's row count at
    /// the end).
    gap: u32,
    /// Its values: at least one, unless put back by value. A row put back
    /// by value holds the file's fields as their bytes (task 2.4c).
    fields: Vec<Value>,
}

impl InsertedRow {
    /// A row of `values` (an empty row has one empty cell, as a blank line
    /// reads), inserted before physical row `gap`.
    pub(crate) fn new(gap: u32, values: &[Value]) -> InsertedRow {
        let mut fields = values.to_vec();
        if fields.is_empty() {
            fields.push(Value::from(""));
        }
        InsertedRow { gap, fields }
    }

    /// A row of exactly `values`, inserted before physical row `gap`: a
    /// row put back by value, which column deletes may have left with none.
    pub(crate) fn exactly(gap: u32, values: &[Value]) -> InsertedRow {
        InsertedRow {
            gap,
            fields: values.to_vec(),
        }
    }

    pub(crate) fn gap(&self) -> u32 {
        self.gap
    }

    pub(crate) fn fields(&self) -> &[Value] {
        &self.fields
    }
}

/// A row an edit touched, with what it is now: its edits (none: it has
/// none, or is gone) and, for an inserted row still there, the row.
#[derive(Clone, Debug)]
pub(crate) struct Touched {
    pub(crate) id: RowId,
    pub(crate) edits: Option<Arc<RowEdits>>,
    pub(crate) inserted: Option<Arc<InsertedRow>>,
}

/// Every edited row, by [`RowId`], the inserted rows, and the piece list
/// that says which row each logical row is. A snapshot: edits make a new
/// one ([`EditStore`]), so a reader holding this one (a copy, ADR-0008
/// decision 2) sees the rows as they were when it took it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overlay {
    rows: BTreeMap<RowId, Arc<RowEdits>>,
    inserted: BTreeMap<u32, Arc<InsertedRow>>,
    map: RowMap,
    /// The column inserts and deletes in effect (task 2.4b).
    columns: Arc<Columns>,
}

/// What a reader needs to lay out one row's cells: its id, its edits and
/// the column operations ([`Overlay::cells_of`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct OverlayRow<'a> {
    pub(crate) id: RowId,
    pub(crate) edits: Option<&'a RowEdits>,
    pub(crate) columns: &'a Columns,
}

impl Overlay {
    /// An overlay of `rows`' edits alone, to read original rows with edits
    /// a command holds (`Rows::values`).
    pub(crate) fn of_rows(
        rows: impl IntoIterator<Item = (RowId, Arc<RowEdits>)>,
        columns: Arc<Columns>,
    ) -> Overlay {
        Overlay {
            rows: rows.into_iter().collect(),
            columns,
            ..Overlay::default()
        }
    }

    /// True if nothing is edited: no cell, no row inserted or deleted, and
    /// no column.
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.map.is_identity() && self.columns.is_empty()
    }

    /// The column inserts and deletes in effect.
    pub(crate) fn columns(&self) -> &Arc<Columns> {
        &self.columns
    }

    /// Row `id`'s edits and the column operations, for a `RowView`.
    pub(crate) fn cells_of(&self, id: RowId) -> OverlayRow<'_> {
        OverlayRow {
            id,
            edits: self.edits(id).map(AsRef::as_ref),
            columns: &self.columns,
        }
    }

    /// Physical row `row`'s, as [`cells_of`](Self::cells_of).
    pub(crate) fn physical(&self, row: usize) -> OverlayRow<'_> {
        let id = RowId::original(u32::try_from(row).unwrap_or(u32::MAX));
        self.cells_of(id)
    }

    /// Which row each logical row is.
    pub(crate) fn map(&self) -> &RowMap {
        &self.map
    }

    /// Physical row `row`'s edits, if it has any.
    pub(crate) fn row(&self, row: usize) -> Option<&RowEdits> {
        if self.rows.is_empty() {
            return None;
        }
        self.row_arc(row).map(AsRef::as_ref)
    }

    /// Physical row `row`'s edits, shared, if it has any.
    pub(crate) fn row_arc(&self, row: usize) -> Option<&Arc<RowEdits>> {
        let row = u32::try_from(row).ok()?;
        self.rows.get(&RowId::original(row))
    }

    /// Row `id`'s edits, if it has any.
    pub(crate) fn edits(&self, id: RowId) -> Option<&Arc<RowEdits>> {
        if self.rows.is_empty() {
            return None;
        }
        self.rows.get(&id)
    }

    /// Inserted row `n`, if it is in the document.
    pub(crate) fn inserted(&self, n: u32) -> Option<&Arc<InsertedRow>> {
        self.inserted.get(&n)
    }

    /// Every inserted row in the document, by number.
    pub(crate) fn inserted_rows(&self) -> impl Iterator<Item = (u32, &Arc<InsertedRow>)> {
        self.inserted.iter().map(|(&n, row)| (n, row))
    }

    /// True if physical row `row` has edits.
    pub(crate) fn contains(&self, row: usize) -> bool {
        self.row_arc(row).is_some()
    }

    /// The edited original rows among physical rows `rows`, in order.
    pub(crate) fn rows_in(
        &self,
        rows: Range<usize>,
    ) -> impl DoubleEndedIterator<Item = (usize, &RowEdits)> {
        // A reversed range would panic: it is empty.
        let start = RowId::original_bound(rows.start);
        let end = RowId::original_bound(rows.end).max(start);
        self.rows.range(start..end).map(|(&id, edits)| {
            let row = id.physical().map_or(usize::MAX, to_usize);
            (row, edits.as_ref())
        })
    }

    /// The edits of rows `ids`, shared, in order.
    pub(crate) fn edits_in(
        &self,
        ids: Range<RowId>,
    ) -> impl Iterator<Item = (RowId, &Arc<RowEdits>)> {
        self.rows.range(ids).map(|(&id, edits)| (id, edits))
    }

    /// Every edited row, in order (original rows first, by physical row).
    pub(crate) fn all(&self) -> impl Iterator<Item = (RowId, &RowEdits)> {
        self.rows.iter().map(|(&id, edits)| (id, edits.as_ref()))
    }

    fn set(&mut self, id: RowId, edits: Option<Arc<RowEdits>>) {
        match edits {
            Some(edits) => {
                self.rows.insert(id, edits);
            }
            None => {
                self.rows.remove(&id);
            }
        }
    }
}

/// A row insert or delete, ready to be made ([`EditStore::change_rows`]):
/// the piece list afterwards, the rows' edits and inserted rows that come
/// or go, and the rows to log.
#[derive(Debug)]
pub(crate) struct RowChange {
    pub(crate) map: RowMap,
    pub(crate) edits: Vec<(RowId, Option<Arc<RowEdits>>)>,
    pub(crate) inserted: Vec<(u32, Option<Arc<InsertedRow>>)>,
    /// The rows inserted, deleted or restored, as runs of ids.
    pub(crate) touched: Vec<Range<RowId>>,
    /// The next inserted row's number afterwards.
    pub(crate) next_inserted: u32,
}

/// A column insert or delete, made or undone ([`EditStore::change_columns`]):
/// the operations afterwards, and the rows whose edits change with it.
#[derive(Debug)]
pub(crate) struct ColumnChange {
    pub(crate) columns: Arc<Columns>,
    pub(crate) edits: Vec<(RowId, Option<Arc<RowEdits>>)>,
}

/// A document's edits, in one [`Lineage`], shared by its readings while
/// they split the file the same way (the header toggle and a drive
/// reconnecting keep them, ADR-0008 decision 4), and by the jobs reading
/// it.
///
/// It holds the current [`Overlay`] and a log of the rows each edit
/// touched. A reader takes the overlay as a snapshot (an `Arc`, so it costs
/// a reference count), or only the rows it needs (`rows_in`, `since`), so
/// that a long job never holds the whole map and an edit meanwhile doesn't
/// copy it. A search catches up by recounting the rows logged since it
/// last looked. Edits are made one at a time (the document serializes
/// them), so the log's order is the edits' order.
///
/// The log has an entry per run of consecutive row ids an edit touched (a
/// row, for a cell edit; a run, for a row insert or delete), 16 bytes, for
/// the document's life: 16 MB after a million cell edits. The version
/// counts rows, not entries, so a big delete is a big step.
#[derive(Debug)]
pub(crate) struct EditStore {
    lineage: Lineage,
    /// Which file the row ids are of (task 2.4a): new for every store, so
    /// a structural command from another works by value.
    base: BaseId,
    /// The edits start from a file Leal saved in this lineage (task 2.2's
    /// rebase), so the lineage's earlier commands may name a missing cell
    /// that is a field now (`document::editing::holds`).
    rebased: bool,
    state: RwLock<EditState>,
    /// How many edits had to copy the overlay, for tests.
    #[cfg(test)]
    copies: std::sync::atomic::AtomicUsize,
}

impl Default for EditStore {
    /// No edits, in a new lineage.
    fn default() -> Self {
        EditStore::with_lineage(Lineage::new())
    }
}

#[derive(Debug, Default)]
struct EditState {
    overlay: Arc<Overlay>,
    /// The runs of row ids each edit touched, in order.
    log: Vec<(RowId, u32)>,
    /// `ends[i]`: the rows logged in `log[..=i]`. The store's version is
    /// `start` plus the last.
    ends: Vec<usize>,
    /// The version the log starts from: a store that replaces another
    /// (a save's rebase, a re-read with another split) carries on from its
    /// version, so a document's edit versions only ever increase.
    start: usize,
    /// How many cells are edited and shown, or `None` once a column
    /// insert or delete may have changed which are shown: counted again
    /// when asked ([`EditStore::cells`]), not by the operation, which would
    /// cost it a look at every edited row's cells.
    cells: Option<usize>,
    /// The next inserted row's number.
    next_inserted: u32,
    /// The next column operation's number.
    next_op: u32,
    /// How many column inserts and deletes have been made or undone: a
    /// search starts again when it changes (ADR-0014 decision 2).
    column_generation: u64,
}

impl EditState {
    fn version(&self) -> usize {
        self.start + self.ends.last().copied().unwrap_or(0)
    }

    fn log(&mut self, first: RowId, count: u32) {
        log_run(&mut self.log, &mut self.ends, first, count);
    }
}

/// [`EditState::log`], over the log's own fields, for when others are
/// borrowed too.
fn log_run(log: &mut Vec<(RowId, u32)>, ends: &mut Vec<usize>, first: RowId, count: u32) {
    let total = ends.last().copied().unwrap_or(0) + to_usize(count);
    log.push((first, count));
    ends.push(total);
}

/// `cells` edited cells, once row `id`'s edits `before` are `after`, under
/// `columns`: only the cells the row shows count, not those a column
/// delete hides.
fn counted(
    cells: usize,
    columns: &Columns,
    id: RowId,
    before: Option<&Arc<RowEdits>>,
    after: Option<&Arc<RowEdits>>,
) -> usize {
    let shown = |edits: Option<&Arc<RowEdits>>| {
        edits.map_or(0, |e| e.shown_len(columns, id.inserted_index()))
    };
    cells - shown(before) + shown(after)
}

impl EditStore {
    /// No edits, in `lineage`: a new reading's, or, after task 2.2's save
    /// rebases the document onto the saved file, the old edits' own, so
    /// that the undo history carries on.
    pub(crate) fn with_lineage(lineage: Lineage) -> EditStore {
        EditStore {
            lineage,
            base: BaseId::new(),
            rebased: false,
            state: RwLock::default(),
            #[cfg(test)]
            copies: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// No edits, in `lineage`, over the file a save in that lineage just
    /// wrote: the undo history carries on (ADR-0008 decision 1).
    pub(crate) fn rebased(lineage: Lineage) -> EditStore {
        EditStore {
            rebased: true,
            ..EditStore::with_lineage(lineage)
        }
    }

    /// Whether the edits start from a file Leal saved in their lineage.
    pub(crate) fn is_rebased(&self) -> bool {
        self.rebased
    }

    /// The lineage of every command made in these edits.
    pub(crate) fn lineage(&self) -> Lineage {
        self.lineage
    }

    /// The file the edits' row ids are of.
    pub(crate) fn base(&self) -> BaseId {
        self.base
    }

    /// The edits now.
    pub(crate) fn overlay(&self) -> Arc<Overlay> {
        Arc::clone(&self.read().overlay)
    }

    /// The piece list now (a snapshot that shares its leaves, without the
    /// rest of the overlay).
    #[cfg(test)]
    pub(crate) fn map(&self) -> RowMap {
        self.read().overlay.map.clone()
    }

    /// How many logical rows can be read when the first `available`
    /// physical rows can ([`RowMap::rows_within`]).
    pub(crate) fn rows_within(&self, available: usize) -> usize {
        self.read().overlay.map.rows_within(available)
    }

    /// A count of physical rows as logical rows ([`RowMap::shift`]).
    pub(crate) fn shift(&self, physical: usize) -> usize {
        self.read().overlay.map.shift(physical)
    }

    /// The logical row count, once a row has been inserted or deleted.
    pub(crate) fn map_len(&self) -> Option<usize> {
        self.read().overlay.map.len()
    }

    /// The edits now and their version, in one look: what a save writes,
    /// and where it carries later edits over from (task 2.2).
    pub(crate) fn snapshot(&self) -> (Arc<Overlay>, usize) {
        let state = self.read();
        (Arc::clone(&state.overlay), state.version())
    }

    /// [`snapshot`](Self::snapshot), and the column generation, in one
    /// look: where a search starts (again) from.
    pub(crate) fn snapshot_columns(&self) -> (Arc<Overlay>, usize, u64) {
        let state = self.read();
        (
            Arc::clone(&state.overlay),
            state.version(),
            state.column_generation,
        )
    }

    /// How many edits have been made: the version the edits are.
    pub(crate) fn version(&self) -> usize {
        self.read().version()
    }

    /// The next column operation's number.
    pub(crate) fn next_op(&self) -> u32 {
        self.read().next_op
    }

    /// How many column inserts and deletes have been made or undone.
    pub(crate) fn column_generation(&self) -> u64 {
        self.read().column_generation
    }

    /// The column operations now.
    pub(crate) fn columns(&self) -> Arc<Columns> {
        Arc::clone(&self.read().overlay.columns)
    }

    /// The next inserted row's number.
    pub(crate) fn next_inserted(&self) -> u32 {
        self.read().next_inserted
    }

    /// Makes the version `version` now, as the store this one replaces had
    /// reached it: edits already logged here (a save's carry-over, made
    /// before the store is current) are counted within it, not after it.
    pub(crate) fn carry_on_from(&self, version: usize) {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let logged = state.ends.last().copied().unwrap_or(0);
        state.start = version.saturating_sub(logged);
    }

    /// The edited original rows among physical rows `rows` now, the
    /// stretches of them that are still in the document (not deleted), and
    /// the version they are: all in one look.
    pub(crate) fn rows_in(&self, rows: Range<usize>) -> RowsIn {
        let state = self.read();
        let range = rows.start..rows.end.max(rows.start);
        let edited = state
            .overlay
            .rows_in(range.clone())
            .filter_map(|(row, _)| Some((row, Arc::clone(state.overlay.row_arc(row)?))))
            .collect();
        let to_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let live = state
            .overlay
            .map
            .originals_in(to_u32(range.start)..to_u32(range.end))
            .into_iter()
            .map(|r| to_usize(r.start)..to_usize(r.end))
            .collect();
        RowsIn {
            edited,
            live,
            version: state.version(),
            columns: Arc::clone(&state.overlay.columns),
            column_generation: state.column_generation,
        }
    }

    /// The rows touched by every edit since version `at`, each once, in
    /// id order, with what they are now, the piece list now, and the
    /// version now.
    pub(crate) fn since(&self, at: usize) -> (Vec<Touched>, RowMap, usize) {
        let state = self.read();
        let from = at.saturating_sub(state.start);
        // The first entry not wholly before `from`.
        let first = state.ends.partition_point(|&end| end <= from);
        let mut ids: Vec<RowId> = state.log[first..]
            .iter()
            .flat_map(|&(id, count)| (0..count).map(move |k| id.plus(k)))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let overlay = &state.overlay;
        let touched = ids
            .into_iter()
            .map(|id| Touched {
                id,
                edits: overlay.edits(id).cloned(),
                inserted: id
                    .inserted_index()
                    .and_then(|n| overlay.inserted(n).cloned()),
            })
            .collect();
        (touched, overlay.map.clone(), state.version())
    }

    /// True if nothing is edited.
    pub(crate) fn is_empty(&self) -> bool {
        self.read().overlay.is_empty()
    }

    /// How many cells are edited (and shown: not those a column delete
    /// hides). After a column insert or delete, the first call counts
    /// them again, a look at every edited row.
    pub(crate) fn cells(&self) -> usize {
        if let Some(cells) = self.read().cells {
            return cells;
        }
        // Not `write`, which counts an edit's copy of the overlay: nothing
        // here changes it.
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        let overlay = &state.overlay;
        let columns = &overlay.columns;
        let cells = overlay
            .rows
            .iter()
            .map(|(id, edits)| edits.shown_len(columns, id.inserted_index()))
            .sum();
        state.cells = Some(cells);
        cells
    }

    /// Replaces each row's edits (`None`: it has none any more), as one
    /// edit. The overlay is copied first if a reader holds it (a copy in
    /// progress): the copy is of the `Arc`s of the edited rows, not of
    /// their values.
    pub(crate) fn set_rows(&self, rows: Vec<(RowId, Option<RowEdits>)>) {
        let mut state = self.write();
        let EditState {
            overlay,
            log,
            ends,
            cells,
            ..
        } = &mut *state;
        let overlay = Arc::make_mut(overlay);
        for (id, edits) in rows {
            let edits = edits.map(Arc::new);
            if let Some(cells) = cells {
                let columns = &overlay.columns;
                *cells = counted(*cells, columns, id, overlay.rows.get(&id), edits.as_ref());
            }
            overlay.set(id, edits);
            log_run(log, ends, id, 1);
        }
    }

    /// Makes a row insert or delete (task 2.4a), as one edit.
    /// Makes a column insert or delete, or undoes one: the operations
    /// become `change.columns` and the rows' edits change. The rows are
    /// logged (one, row 0, if none changed, so the version goes up); a
    /// search starts again anyway.
    pub(crate) fn change_columns(&self, change: ColumnChange, next_op: u32) {
        let mut state = self.write();
        let overlay = Arc::make_mut(&mut state.overlay);
        overlay.columns = change.columns;
        let mut touched = Vec::with_capacity(change.edits.len());
        for (id, edits) in change.edits {
            overlay.set(id, edits);
            touched.push(id);
        }
        // Which edits a row shows changes in rows the operation left
        // alone too (a delete hides the edits in its column): they are
        // counted again when next asked.
        state.cells = None;
        if touched.is_empty() {
            touched.push(RowId::original(0));
        }
        for id in touched {
            state.log(id, 1);
        }
        state.next_op = state.next_op.max(next_op);
        state.column_generation += 1;
    }

    pub(crate) fn change_rows(&self, change: RowChange) {
        let mut state = self.write();
        let mut cells = state.cells;
        let overlay = Arc::make_mut(&mut state.overlay);
        overlay.map = change.map;
        for (id, edits) in change.edits {
            if let Some(cells) = &mut cells {
                let columns = &overlay.columns;
                *cells = counted(*cells, columns, id, overlay.rows.get(&id), edits.as_ref());
            }
            overlay.set(id, edits);
        }
        for (n, row) in change.inserted {
            match row {
                Some(row) => {
                    overlay.inserted.insert(n, row);
                }
                None => {
                    overlay.inserted.remove(&n);
                }
            }
        }
        state.cells = cells;
        for ids in change.touched {
            let count = ids.start.until(ids.end);
            state.log(ids.start, count);
        }
        state.next_inserted = change.next_inserted;
    }

    /// How many edits so far had to copy the overlay.
    #[cfg(test)]
    pub(crate) fn copies(&self) -> usize {
        self.copies.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn read(&self) -> RwLockReadGuard<'_, EditState> {
        // Only whole values are stored, so a poisoned lock still holds a
        // good one.
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, EditState> {
        let state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        // `Arc::make_mut` copies the overlay if a reader holds it.
        #[cfg(test)]
        if Arc::strong_count(&state.overlay) > 1 {
            self.copies
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        state
    }
}

/// What [`EditStore::rows_in`] gives a search's chunk.
#[derive(Debug)]
pub(crate) struct RowsIn {
    /// The edited original rows, by physical row, in order.
    pub(crate) edited: Vec<(usize, Arc<RowEdits>)>,
    /// The stretches of physical rows still in the document, in order.
    pub(crate) live: Vec<Range<usize>>,
    pub(crate) version: usize,
    /// The column operations, and their generation.
    pub(crate) columns: Arc<Columns>,
    pub(crate) column_generation: u64,
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cell `c` of a row of `fields` fields with no column operation.
    fn at(c: usize, fields: usize) -> CellId {
        CellId::base(c, fields)
    }

    fn edits(fields: usize, cells: &[(usize, &str)]) -> RowEdits {
        let cells = cells
            .iter()
            .map(|&(c, v)| (at(c, fields), Arc::from(v)))
            .collect();
        RowEdits::new(fields, cells, Arc::new([]), None)
    }

    fn id(row: u32) -> RowId {
        RowId::original(row)
    }

    #[test]
    fn a_row_is_as_long_as_its_last_edited_cell() {
        let none = Columns::default();
        assert_eq!(edits(3, &[(1, "x")]).len_in(&none, None), 3);
        assert_eq!(edits(1, &[(4, "x")]).len_in(&none, None), 5);
        let row = edits(2, &[(0, "a"), (5, "b")]);
        assert_eq!(row.get(at(0, 2)), Some("a"));
        assert_eq!(row.get(at(5, 2)), Some("b"));
        assert_eq!(row.get(at(1, 2)), None);
        let shown: Vec<usize> = row.shown(&none, None).iter().map(|&(c, _)| c).collect();
        assert_eq!(shown, [0, 5]);
    }

    /// An edited field's own diagnostics go; the others stay; a NUL in an
    /// edited value is one.
    #[test]
    fn a_rows_kinds_are_its_unedited_fields_and_its_new_values() {
        let flagged: Arc<[(usize, Kinds)]> =
            Arc::new([(0, Kinds::INVALID_ENCODING), (2, Kinds::NUL_BYTES)]);
        let row = |cells: &[(usize, &str)]| {
            let cells = cells
                .iter()
                .map(|&(c, v)| (at(c, 3), Arc::from(v)))
                .collect();
            RowEdits::new(3, cells, Arc::clone(&flagged), None)
                .kinds_in(&Columns::default(), RowId::original(0))
        };
        let both = row(&[(1, "x")]);
        assert!(both.contains(Kinds::INVALID_ENCODING) && both.contains(Kinds::NUL_BYTES));
        assert_eq!(row(&[(0, "x")]), Kinds::NUL_BYTES);
        assert_eq!(row(&[(0, "x"), (2, "y")]), Kinds::default());
        assert_eq!(row(&[(0, "x"), (2, "\0")]), Kinds::NUL_BYTES);
    }

    #[test]
    fn kinds_combine() {
        let mut kinds = Kinds::default();
        assert!(kinds.is_empty());
        kinds.insert(Kinds::NUL_BYTES);
        assert!(kinds.contains(Kinds::NUL_BYTES));
        assert!(!kinds.contains(Kinds::INVALID_ENCODING));
        assert!(!kinds.contains(Kinds::default()));
        assert_eq!(Kinds::of(DiagnosticKind::NulBytes), Kinds::NUL_BYTES);
        assert_eq!(Kinds::of(DiagnosticKind::RaggedRows), Kinds::default());
    }

    #[test]
    fn the_store_logs_each_edit_and_keeps_snapshots_as_they_were() {
        let store = EditStore::default();
        assert!(store.is_empty());
        let before = store.overlay();
        store.set_rows(vec![(id(7), Some(edits(2, &[(0, "x"), (1, "y")])))]);
        store.set_rows(vec![(id(2), Some(edits(2, &[(1, "y")]))), (id(7), None)]);
        assert!(before.is_empty(), "a snapshot doesn't change");
        assert_eq!(store.version(), 3);
        assert_eq!(store.cells(), 1);
        let overlay = store.overlay();
        assert!(overlay.row(2).is_some() && overlay.row(7).is_none());
        let (rows, _, version) = store.since(1);
        let rows: Vec<(RowId, bool)> = rows
            .into_iter()
            .map(|t| (t.id, t.edits.is_some()))
            .collect();
        assert_eq!((rows, version), (vec![(id(2), true), (id(7), false)], 3));
        assert!(store.since(3).0.is_empty());
        assert!(store.since(99).0.is_empty());
        assert_ne!(store.lineage(), EditStore::default().lineage());
        assert_ne!(store.base(), EditStore::default().base());
    }

    #[test]
    fn rows_in_gives_the_edited_rows_in_a_range_either_way() {
        let store = EditStore::default();
        for row in [1, 4, 9] {
            store.set_rows(vec![(id(row), Some(edits(1, &[(0, "x")])))]);
        }
        let overlay = store.overlay();
        let rows = |range: Range<usize>| overlay.rows_in(range).map(|(r, _)| r).collect::<Vec<_>>();
        assert_eq!(rows(0..10), [1, 4, 9]);
        assert_eq!(rows(4..9), [4]);
        assert_eq!(rows(5..5), Vec::<usize>::new());
        let (start, end) = (9, 2);
        assert_eq!(rows(start..end), Vec::<usize>::new(), "reversed");
        let back: Vec<usize> = overlay.rows_in(0..9).rev().map(|(r, _)| r).collect();
        assert_eq!(back, [4, 1]);
        let shared = store.rows_in(start..end);
        assert!(shared.edited.is_empty());
        assert_eq!(shared.version, 3);
        assert_eq!(store.rows_in(2..10).edited.len(), 2);
        assert_eq!(store.rows_in(2..10).live, vec![2..10]);
    }

    /// A row insert or delete logs its rows as one run, which the version
    /// counts row by row; the edits it moves go and come back whole.
    #[test]
    fn a_row_change_logs_its_run_and_moves_edits() {
        let store = EditStore::default();
        store.set_rows(vec![(id(3), Some(edits(2, &[(0, "x")])))]);
        let mut map = store.map();
        map.begin(10);
        let removed = map.remove(2..6);
        let moved = Arc::clone(store.overlay().edits(id(3)).unwrap());
        store.change_rows(RowChange {
            map,
            edits: vec![(id(3), None)],
            inserted: vec![],
            touched: removed.iter().map(|p| p.ids()).collect(),
            next_inserted: 0,
        });
        assert_eq!(store.version(), 5, "one cell edit, then four rows");
        assert_eq!(store.cells(), 0);
        assert!(!store.is_empty(), "rows are deleted");
        let (touched, map, _) = store.since(1);
        assert_eq!(touched.len(), 4);
        assert_eq!(map.len(), Some(6));
        assert_eq!(store.rows_in(0..10).live, vec![0..2, 6..10]);
        let mut back = store.map();
        assert!(back.insert(2, &removed));
        store.change_rows(RowChange {
            map: back,
            edits: vec![(id(3), Some(moved))],
            inserted: vec![],
            touched: removed.iter().map(|p| p.ids()).collect(),
            next_inserted: 0,
        });
        assert!(!store.is_empty());
        assert_eq!(store.cells(), 1);
        assert!(store.map().is_identity());
        store.set_rows(vec![(id(3), None)]);
        assert!(store.is_empty());
    }

    #[test]
    fn an_inserted_row_has_at_least_one_cell() {
        let row = InsertedRow::new(4, &[]);
        assert_eq!(row.fields().len(), 1);
        assert_eq!(row.gap(), 4);
        let store = EditStore::default();
        let mut map = store.map();
        map.begin(0);
        assert!(map.insert(
            0,
            &[Piece::Inserted {
                gap: 0,
                first: 0,
                len: 1
            }]
        ));
        store.change_rows(RowChange {
            map,
            edits: vec![],
            inserted: vec![(0, Some(Arc::new(row)))],
            touched: vec![RowId::inserted(0)..RowId::inserted(1)],
            next_inserted: 1,
        });
        assert_eq!(store.next_inserted(), 1);
        assert!(store.overlay().inserted(0).is_some());
        let (touched, _, _) = store.since(0);
        assert!(touched[0].inserted.is_some());
    }
}
