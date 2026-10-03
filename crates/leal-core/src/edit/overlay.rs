//! The overlay: what the commands changed, by row id, the inserted rows
//! and the piece list, and the store that shares it between a document's
//! readings and its jobs.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::Lineage;
#[cfg(test)]
use super::rows::Piece;
use super::rows::{RowId, RowMap};
use super::structural::BaseId;
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

/// One edited row: its new cell values, and what its diagnostics are now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowEdits {
    /// How many fields the row has in the file (as it was read when the
    /// row was first edited).
    fields: usize,
    /// The edited cells, sorted by column: new values for some of the
    /// row's fields, or for cells past its end. Never empty.
    cells: Vec<(usize, Arc<str>)>,
    /// The row's own fields that have field-level diagnostics, worked out
    /// from their bytes once, when the row was first edited, and shared by
    /// every later version of its edits: so an edit costs what its cells
    /// do, not the whole row (usually empty).
    flagged: Arc<[(usize, Kinds)]>,
    /// The field-level diagnostics of the row as it reads now: each
    /// unedited field's from `flagged`, each edited cell on its new value
    /// (ADR-0008 decision 2).
    kinds: Kinds,
    /// A hash of the row's bytes, if it was read from the first 64 KB kept
    /// in memory: if those turn out to be a different version of the file
    /// (task 1.9), the edit is checked against the row in the trusted copy
    /// (`Document::edit_conflicts`).
    head_hash: Option<u64>,
}

impl RowEdits {
    /// A row of `fields` fields in the file, with `cells` edited (sorted by
    /// column, not empty) and its own fields' diagnostics `flagged`.
    pub(crate) fn new(
        fields: usize,
        cells: Vec<(usize, Arc<str>)>,
        flagged: Arc<[(usize, Kinds)]>,
        head_hash: Option<u64>,
    ) -> RowEdits {
        debug_assert!(!cells.is_empty());
        debug_assert!(cells.windows(2).all(|pair| pair[0].0 < pair[1].0));
        let mut kinds = Kinds::default();
        for &(column, bits) in flagged.iter() {
            if cells.binary_search_by_key(&column, |&(c, _)| c).is_err() {
                kinds.insert(bits);
            }
        }
        if cells.iter().any(|(_, value)| value.contains('\0')) {
            kinds.insert(Kinds::NUL_BYTES);
        }
        RowEdits {
            fields,
            cells,
            flagged,
            kinds,
            head_hash,
        }
    }

    /// How many fields of its own the row had when it was first edited.
    pub(crate) fn fields(&self) -> usize {
        self.fields
    }

    /// One past the last edited cell.
    pub(crate) fn end(&self) -> usize {
        self.cells.last().map_or(0, |&(column, _)| column + 1)
    }

    /// How many cells the row has now, against the field count it had when
    /// it was first edited (the marks use it; readers count the row's own
    /// fields as they read them, `RowView::len`).
    pub(crate) fn len(&self) -> usize {
        self.fields.max(self.end())
    }

    /// The new value of cell `column`, if it is edited.
    pub(crate) fn get(&self, column: usize) -> Option<&str> {
        let at = self.cells.binary_search_by_key(&column, |&(c, _)| c).ok()?;
        Some(&self.cells[at].1)
    }

    /// The edited cells, sorted by column.
    pub(crate) fn cells(&self) -> &[(usize, Arc<str>)] {
        &self.cells
    }

    pub(crate) fn flagged(&self) -> &Arc<[(usize, Kinds)]> {
        &self.flagged
    }

    pub(crate) fn kinds(&self) -> Kinds {
        self.kinds
    }

    pub(crate) fn head_hash(&self) -> Option<u64> {
        self.head_hash
    }
}

/// An inserted row (task 2.4a): its own values, and the gap it was
/// inserted at, for life. Its cell edits are [`RowEdits`] like any row's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InsertedRow {
    /// The physical row it was inserted before (the file's row count at
    /// the end).
    gap: u32,
    /// Its values, at least one.
    fields: Vec<Arc<str>>,
}

impl InsertedRow {
    /// A row of `values` (an empty row has one empty cell, as a blank line
    /// reads), inserted before physical row `gap`.
    pub(crate) fn new(gap: u32, values: &[String]) -> InsertedRow {
        let mut fields: Vec<Arc<str>> = values.iter().map(|v| Arc::from(v.as_str())).collect();
        if fields.is_empty() {
            fields.push(Arc::from(""));
        }
        InsertedRow { gap, fields }
    }

    pub(crate) fn gap(&self) -> u32 {
        self.gap
    }

    pub(crate) fn fields(&self) -> &[Arc<str>] {
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
}

impl Overlay {
    /// An overlay of `rows`' edits alone, to read original rows with edits
    /// a command holds (`Rows::values`).
    pub(crate) fn of_rows(rows: impl IntoIterator<Item = (RowId, Arc<RowEdits>)>) -> Overlay {
        Overlay {
            rows: rows.into_iter().collect(),
            ..Overlay::default()
        }
    }

    /// True if nothing is edited: no cell, and no row inserted or deleted.
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.map.is_identity()
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
    /// How many cells are edited.
    cells: usize,
    /// The next inserted row's number.
    next_inserted: u32,
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

/// `cells` edited cells, once a row's edits `before` are `after`.
fn counted(cells: usize, before: Option<&Arc<RowEdits>>, after: Option<&Arc<RowEdits>>) -> usize {
    let before = before.map_or(0, |e| e.cells.len());
    let after = after.map_or(0, |e| e.cells.len());
    cells - before + after
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

    /// How many edits have been made: the version the edits are.
    pub(crate) fn version(&self) -> usize {
        self.read().version()
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

    /// Physical row `row`'s edits now, if it has any.
    pub(crate) fn row(&self, row: usize) -> Option<Arc<RowEdits>> {
        self.read().overlay.row_arc(row).cloned()
    }

    /// Inserted row `n` now, if it is in the document, with its edits.
    pub(crate) fn inserted(&self, n: u32) -> Option<(Arc<InsertedRow>, Option<Arc<RowEdits>>)> {
        let state = self.read();
        let row = Arc::clone(state.overlay.inserted(n)?);
        let edits = state.overlay.edits(RowId::inserted(n)).cloned();
        Some((row, edits))
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

    /// How many cells are edited.
    pub(crate) fn cells(&self) -> usize {
        self.read().cells
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
            *cells = counted(*cells, overlay.rows.get(&id), edits.as_ref());
            overlay.set(id, edits);
            log_run(log, ends, id, 1);
        }
    }

    /// Makes a row insert or delete (task 2.4a), as one edit.
    pub(crate) fn change_rows(&self, change: RowChange) {
        let mut state = self.write();
        let mut cells = state.cells;
        let overlay = Arc::make_mut(&mut state.overlay);
        overlay.map = change.map;
        for (id, edits) in change.edits {
            cells = counted(cells, overlay.rows.get(&id), edits.as_ref());
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
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(fields: usize, cells: &[(usize, &str)]) -> RowEdits {
        let cells = cells.iter().map(|&(c, v)| (c, Arc::from(v))).collect();
        RowEdits::new(fields, cells, Arc::new([]), None)
    }

    fn id(row: u32) -> RowId {
        RowId::original(row)
    }

    #[test]
    fn a_row_is_as_long_as_its_last_edited_cell() {
        assert_eq!(edits(3, &[(1, "x")]).len(), 3);
        assert_eq!(edits(1, &[(4, "x")]).len(), 5);
        assert_eq!(edits(1, &[(4, "x")]).end(), 5);
        let row = edits(2, &[(0, "a"), (5, "b")]);
        assert_eq!(row.get(0), Some("a"));
        assert_eq!(row.get(5), Some("b"));
        assert_eq!(row.get(1), None);
    }

    /// An edited field's own diagnostics go; the others stay; a NUL in an
    /// edited value is one.
    #[test]
    fn a_rows_kinds_are_its_unedited_fields_and_its_new_values() {
        let flagged: Arc<[(usize, Kinds)]> =
            Arc::new([(0, Kinds::INVALID_ENCODING), (2, Kinds::NUL_BYTES)]);
        let row = |cells: &[(usize, &str)]| {
            let cells = cells.iter().map(|&(c, v)| (c, Arc::from(v))).collect();
            RowEdits::new(3, cells, Arc::clone(&flagged), None).kinds()
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
        assert!(store.row(2).is_some() && store.row(7).is_none());
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
        assert!(store.inserted(0).is_some());
        let (touched, _, _) = store.since(0);
        assert!(touched[0].inserted.is_some());
    }
}
