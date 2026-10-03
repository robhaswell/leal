//! The overlay: what the commands changed, by physical row, and the store
//! that shares it between a document's readings and its jobs.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use super::Lineage;
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

/// A row an edit touched, with its edits now (none: it has none left).
pub(crate) type Touched = (usize, Option<Arc<RowEdits>>);

/// Every edited row, by physical row. A snapshot: edits make a new one
/// ([`EditStore`]), so a reader holding this one (a copy, ADR-0008
/// decision 2) sees the cells as they were when it took it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Overlay {
    rows: BTreeMap<usize, Arc<RowEdits>>,
}

impl Overlay {
    /// True if nothing is edited.
    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Row `row`'s edits, if it has any.
    pub(crate) fn row(&self, row: usize) -> Option<&RowEdits> {
        if self.rows.is_empty() {
            return None;
        }
        self.rows.get(&row).map(AsRef::as_ref)
    }

    /// Row `row`'s edits, shared, if it has any.
    pub(crate) fn row_arc(&self, row: usize) -> Option<&Arc<RowEdits>> {
        self.rows.get(&row)
    }

    /// True if row `row` has edits.
    pub(crate) fn contains(&self, row: usize) -> bool {
        self.rows.contains_key(&row)
    }

    /// The edited rows among `rows`, in order.
    pub(crate) fn rows_in(
        &self,
        rows: Range<usize>,
    ) -> impl DoubleEndedIterator<Item = (usize, &RowEdits)> {
        // A reversed range would panic: it is empty.
        self.rows
            .range(rows.start..rows.end.max(rows.start))
            .map(|(&row, edits)| (row, edits.as_ref()))
    }

    /// Every edited row, in order.
    pub(crate) fn all(&self) -> impl Iterator<Item = (usize, &RowEdits)> {
        self.rows.iter().map(|(&row, edits)| (row, edits.as_ref()))
    }

    fn set(&mut self, row: usize, edits: Option<RowEdits>) {
        match edits {
            Some(edits) => {
                self.rows.insert(row, Arc::new(edits));
            }
            None => {
                self.rows.remove(&row);
            }
        }
    }
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
/// The log grows by one entry (8 bytes) per row each edit touches, for the
/// document's life: 8 MB after a million cell edits.
#[derive(Debug)]
pub(crate) struct EditStore {
    lineage: Lineage,
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
    /// The row each edit touched, in order. The store's version is `base`
    /// plus its length.
    log: Vec<usize>,
    /// The version the log starts from: a store that replaces another
    /// (a save's rebase, a re-read with another split) carries on from its
    /// version, so a document's edit versions only ever increase.
    base: usize,
    /// How many cells are edited.
    cells: usize,
}

impl EditState {
    fn version(&self) -> usize {
        self.base + self.log.len()
    }
}

impl EditStore {
    /// No edits, in `lineage`: a new reading's, or, after task 2.2's save
    /// rebases the document onto the saved file, the old edits' own, so
    /// that the undo history carries on.
    pub(crate) fn with_lineage(lineage: Lineage) -> EditStore {
        EditStore {
            lineage,
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

    /// The edits now.
    pub(crate) fn overlay(&self) -> Arc<Overlay> {
        Arc::clone(&self.read().overlay)
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

    /// Makes the version `version` now, as the store this one replaces had
    /// reached it: edits already logged here (a save's carry-over, made
    /// before the store is current) are counted within it, not after it.
    pub(crate) fn carry_on_from(&self, version: usize) {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        state.base = version.saturating_sub(state.log.len());
    }

    /// Row `row`'s edits now, if it has any.
    pub(crate) fn row(&self, row: usize) -> Option<Arc<RowEdits>> {
        self.read().overlay.row_arc(row).cloned()
    }

    /// The edited rows among `rows` now, and the version they are.
    pub(crate) fn rows_in(&self, rows: Range<usize>) -> (Vec<(usize, Arc<RowEdits>)>, usize) {
        let state = self.read();
        let range = rows.start..rows.end.max(rows.start);
        let edited = state
            .overlay
            .rows
            .range(range)
            .map(|(&row, edits)| (row, Arc::clone(edits)))
            .collect();
        (edited, state.version())
    }

    /// The rows touched by every edit since version `at`, each once, in
    /// order, with their edits now, and the version now.
    pub(crate) fn since(&self, at: usize) -> (Vec<Touched>, usize) {
        let state = self.read();
        let from = at.saturating_sub(state.base);
        let mut rows = state.log.get(from..).unwrap_or_default().to_vec();
        rows.sort_unstable();
        rows.dedup();
        let rows = rows
            .into_iter()
            .map(|row| (row, state.overlay.row_arc(row).cloned()))
            .collect();
        (rows, state.version())
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
    pub(crate) fn set_rows(&self, rows: Vec<(usize, Option<RowEdits>)>) {
        let mut state = self.state.write().unwrap_or_else(PoisonError::into_inner);
        #[cfg(test)]
        if Arc::get_mut(&mut state.overlay).is_none() {
            self.copies
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let state = &mut *state;
        let overlay = Arc::make_mut(&mut state.overlay);
        for (row, edits) in rows {
            let before = overlay.row(row).map_or(0, |e| e.cells.len());
            let after = edits.as_ref().map_or(0, |e| e.cells.len());
            state.cells = state.cells - before + after;
            overlay.set(row, edits);
            state.log.push(row);
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edits(fields: usize, cells: &[(usize, &str)]) -> RowEdits {
        let cells = cells.iter().map(|&(c, v)| (c, Arc::from(v))).collect();
        RowEdits::new(fields, cells, Arc::new([]), None)
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
        store.set_rows(vec![(7, Some(edits(2, &[(0, "x"), (1, "y")])))]);
        store.set_rows(vec![(2, Some(edits(2, &[(1, "y")]))), (7, None)]);
        assert!(before.is_empty(), "a snapshot doesn't change");
        assert_eq!(store.version(), 3);
        assert_eq!(store.cells(), 1);
        assert!(store.row(2).is_some() && store.row(7).is_none());
        let (rows, version) = store.since(1);
        let rows: Vec<(usize, bool)> = rows.into_iter().map(|(r, e)| (r, e.is_some())).collect();
        assert_eq!((rows, version), (vec![(2, true), (7, false)], 3));
        assert!(store.since(3).0.is_empty());
        assert!(store.since(99).0.is_empty());
        assert_ne!(store.lineage(), EditStore::default().lineage());
    }

    #[test]
    fn rows_in_gives_the_edited_rows_in_a_range_either_way() {
        let store = EditStore::default();
        for row in [1, 4, 9] {
            store.set_rows(vec![(row, Some(edits(1, &[(0, "x")])))]);
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
        let (shared, version) = store.rows_in(start..end);
        assert!(shared.is_empty());
        assert_eq!(version, 3);
        assert_eq!(store.rows_in(2..10).0.len(), 2);
    }
}
