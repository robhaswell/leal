//! Inserting and deleting columns (task 2.4b, `docs/tasks/2.4.md` §3, §4
//! and §7): the commands, their undo and redo by identity within the same
//! edits, and by value in other edits (after a save, or in a replay).
//!
//! A column operation applies to each row as oracle rule 6 says (ADR-0004
//! decision 5): an insert at `c` to rows of at least `c` cells, a delete at
//! `c` to rows of more, never to a blank line. Which rows that is depends,
//! for an unedited row, only on its field count (`edit::columns`), so the
//! operation walks the marks' field counts once (about a millisecond per
//! million rows) to record the logical rows it applied to, for an undo by
//! value, and looks at each edited row, whose layout is written out if the
//! rule decides otherwise for it than its default layout would.
//!
//! Like row inserts and deletes, they need the whole file read and no save
//! running (ADR-0014 decision 1), and a search starts again after one
//! (ADR-0014 decision 2).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::structural::{open_quote_row, whole_file};
use super::{Document, Reading, RowBytes, RowView};
use crate::diagnostics::{CountsBuilder, Diagnostics, RowCode, RowMarks};
use crate::edit::{
    CellId, Changed, Column, ColumnChange, ColumnOp, ColumnSource, Columns, Command, Edit,
    EditError, Lineage, OpId, OpKind, Overlay, Own as EditOwn, RowEdits, RowId, Segment,
    fresh_appended,
};
use crate::save::ColumnQuoting;
use crate::source::ReadError;

/// Which rows an operation applies to.
#[derive(Clone, Copy)]
enum Decide<'a> {
    /// Oracle rule 6, each row as it reads now.
    Rule,
    /// Exactly these logical rows: a command's, by value.
    Rows(&'a [Range<u32>]),
}

/// A column operation worked out, ready to be made.
struct Plan {
    op: ColumnOp,
    after: Arc<Columns>,
    /// (row, edits before, edits after) for each row whose edits change.
    rows: Vec<Changed>,
    applied: Vec<Range<u32>>,
}

impl Document {
    /// Inserts a column before logical column `at`, each new cell holding
    /// `value`, as one command for the app's undo history (undo takes the
    /// cells out again). It goes into every row with at least `at` cells
    /// (oracle rule 6, ADR-0004 decision 5): a shorter row, or a blank
    /// line, is left as it is, as is a row that edits and column deletes
    /// have left with no cells (ADR-0014 decision 6). `at` may be the
    /// widest row's length, which
    /// adds a column after the last. A search starts again
    /// (ADR-0014 decision 2).
    ///
    /// It reads no row, but looks at every row's field count in the
    /// marks: about a millisecond per million rows.
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] until the whole file has been read,
    /// [`EditError::Saving`] while a save runs (ADR-0014 decision 1),
    /// [`EditError::NoSuchColumn`] past the widest row, and
    /// [`EditError::AfterUnterminatedQuote`] for a cell after an
    /// unterminated quote that is still open (ADR-0004 decision 8). The
    /// document is then unchanged.
    pub fn insert_column(&self, at: usize, value: &str) -> Result<Option<Command>, EditError> {
        self.change_rows(|reading| {
            let kind = OpKind::Insert(Arc::from(value));
            make(reading, kind, at, Decide::Rule, None)
        })
    }

    /// Deletes logical column `at` from every row that has it, as one
    /// command (undo puts the same cells back, edits and original bytes
    /// included). Deleting a row's last hatched cell takes the padding
    /// before it too, so the row's own bytes come back (ADR-0014 decision
    /// 5, refining ADR-0005 decision 2). A search starts again.
    ///
    /// # Errors
    ///
    /// As for [`insert_column`](Self::insert_column):
    /// [`EditError::NoSuchColumn`] if no row has the column.
    pub fn delete_column(&self, at: usize) -> Result<Option<Command>, EditError> {
        self.change_rows(|reading| make(reading, OpKind::Delete, at, Decide::Rule, None))
    }

    /// Whether a column can be inserted before logical column `at` now,
    /// for the app to enable **Insert Column** (task 2.5a). It walks the
    /// marks as [`insert_column`](Self::insert_column) does.
    ///
    /// # Errors
    ///
    /// What [`insert_column`](Self::insert_column) would refuse with.
    pub fn can_insert_column(&self, at: usize) -> Result<(), EditError> {
        self.can_change_column(OpKind::Insert(Arc::from("")), at)
    }

    /// Whether logical column `at` can be deleted now, for **Delete
    /// Column**.
    ///
    /// # Errors
    ///
    /// What [`delete_column`](Self::delete_column) would refuse with.
    pub fn can_delete_column(&self, at: usize) -> Result<(), EditError> {
        self.can_change_column(OpKind::Delete, at)
    }

    fn can_change_column(&self, kind: OpKind, at: usize) -> Result<(), EditError> {
        if self.saving.load(Ordering::Acquire) {
            return Err(EditError::Saving);
        }
        let reading = self.current();
        whole_file(&reading)?;
        let overlay = reading.edits.overlay();
        plan(&reading, &overlay, kind, at, Decide::Rule).map(|_| ())
    }

    /// Applies a column insert or delete command (`edit`), as
    /// [`apply`](Self::apply) (with its `lineage`) or
    /// [`replay`](Self::replay) (with none) do. In the edits it was made in
    /// it applies by identity: undone, the operation comes out and every
    /// cell it moved is back as it was. Elsewhere it works by value on the
    /// rows it applied to, a delete checking each cell first (ADR-0014
    /// decision 3).
    pub(super) fn apply_column(
        &self,
        lineage: Option<Lineage>,
        edit: &Edit,
    ) -> Result<Option<Command>, EditError> {
        self.change_rows(|reading| {
            let store = &reading.edits;
            if lineage.is_some_and(|lineage| lineage != store.lineage()) {
                return Err(EditError::OtherLineage);
            }
            let (inserting, column) = match edit {
                Edit::InsertColumn { column, .. } => (true, column),
                Edit::DeleteColumn { column, .. } => (false, column),
                _ => return Ok(None),
            };
            // The command makes its operation again (redo), or takes it
            // out (undo).
            let again = inserting == column.op.inserts();
            if column.base == store.base() {
                return if again {
                    put_back(reading, column, edit)
                } else {
                    take_out(reading, column, edit)
                };
            }
            let at = column.op.at;
            let rows = Decide::Rows(&column.applied);
            let values = || {
                column
                    .values()
                    .map_err(|error| EditError::Read { row: 0, error })
            };
            match (&column.op.kind, again) {
                (OpKind::Insert(value), true) => {
                    make(reading, OpKind::Insert(Arc::clone(value)), at, rows, None)
                }
                (OpKind::Restore(_), true) | (OpKind::Delete, false) => {
                    let values = values()?;
                    restore(reading, at, &column.applied, values)
                }
                (OpKind::Delete, true) | (OpKind::Insert(_) | OpKind::Restore(_), false) => {
                    let values = values()?;
                    make(reading, OpKind::Delete, at, rows, Some(&values))
                }
            }
        })
    }
}

impl Document {
    /// The per-column quoting census of the document as it reads now
    /// ([`ColumnQuoting::census`]), for the benchmarks
    /// (`column_edits/census`): how many of the first `columns` columns a
    /// new field would be quoted in. Never in the app (`just
    /// check-no-test-exports`).
    ///
    /// # Errors
    ///
    /// A row can't be read.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn bench_quoting_census(&self, columns: usize) -> Result<usize, ReadError> {
        let reading = self.current();
        let overlay = reading.edits.overlay();
        let go = || Ok::<(), ReadError>(());
        let census = ColumnQuoting::census(&reading, &overlay, &go)?;
        Ok((0..columns).filter(|&column| census.quoted(column)).count())
    }
}

impl ColumnQuoting {
    /// The census for per-column quoting of new fields (ADR-0005 decision
    /// 3, ADR-0014 decision 4), for the writer (task 2.4c): one pass over
    /// the file's rows, each as `overlay` lays it out, a batch at a time,
    /// with a `checkpoint` before each (its error, a cancel, stops it).
    /// Every row counts for whether the file quotes every field; the rows
    /// still in the document count for their columns now.
    ///
    /// It reads every row: about half a second per million rows (the
    /// `column_edits/census` bench). So the writer makes it only when a
    /// save writes a new field (an inserted row's or column's), and it
    /// stops early once the answer can't change: the file doesn't quote
    /// every field, and every column up to the widest row's end has an
    /// unquoted non-empty field, so no column quotes every field. A
    /// census stopped early answers [`quoted`](Self::quoted) and
    /// [`every_field`](Self::every_field) as a whole one would.
    pub(in crate::document) fn census<E: From<ReadError>>(
        reading: &Reading,
        overlay: &Overlay,
        checkpoint: &dyn Fn() -> Result<(), E>,
    ) -> Result<ColumnQuoting, E> {
        const BATCH: usize = 4096;
        let mut census = ColumnQuoting::new();
        let map = overlay.map();
        let rows = Document::rows_index(reading).1;
        // Unknown until every row is marked: no early stop then.
        let widest = widest(reading, overlay);
        let mut start = 0;
        while start < rows {
            checkpoint()?;
            let batch = start..rows.min(start + BATCH);
            Document::read_physical(reading, overlay, batch.clone(), &mut |view| {
                census.add_file_row(&view);
                let live = view
                    .id()
                    .physical()
                    .is_some_and(|row| map.logical_of(row).is_ok());
                if live {
                    census.add(&view);
                }
            })?;
            if widest.is_some_and(|widest| census.settled(widest)) {
                break;
            }
            start = batch.end;
        }
        Ok(census)
    }
}

/// Where a column operation finds each unedited row's field count: the
/// marks, once every row has them, or right after a save, the counts the
/// save handed the file's new reading (task 2.4c), which are the same.
#[derive(Clone, Copy)]
pub(super) enum Counts<'a> {
    Marks(&'a Diagnostics),
    Saved(&'a RowMarks),
}

impl Counts<'_> {
    /// `reading`'s, if every row of its file has one.
    pub(super) fn of(reading: &Reading) -> Option<Counts<'_>> {
        let rows = reading.index.row_count();
        if let Some(saved) = reading.counts.as_ref().filter(|saved| saved.len() == rows) {
            return Some(Counts::Saved(saved));
        }
        // The marks are published a moment after the index's rows.
        let diagnostics = reading.diagnostics.get()?;
        (diagnostics.marked_rows_and_mode().0 >= rows).then_some(Counts::Marks(diagnostics))
    }

    /// Hands each of rows `rows`' codes to `each`, in order.
    fn for_each_code(self, rows: Range<usize>, each: &mut dyn FnMut(usize, RowCode)) {
        match self {
            Counts::Marks(diagnostics) => diagnostics.for_each_code(rows, each),
            Counts::Saved(saved) => saved.for_each_code(rows, each),
        }
    }

    /// Adds rows `rows`' field counts to `counts`, in order: a save's rows
    /// copied as they are (task 2.4c). `false` if any isn't known.
    pub(super) fn copy_counts(self, rows: Range<usize>, counts: &mut CountsBuilder) -> bool {
        match self {
            Counts::Marks(diagnostics) => diagnostics.copy_counts(rows, counts),
            Counts::Saved(saved) => saved.copy_counts(rows, counts),
        }
    }

    /// Row `row`'s code.
    pub(super) fn code_of(self, row: usize) -> Option<RowCode> {
        match self {
            Counts::Marks(diagnostics) => diagnostics.code_of(row),
            Counts::Saved(saved) => saved.code_of(row),
        }
    }
}

/// Makes a column operation, of `kind` at `at`, on the rows `decide`
/// says; a delete by value first checks each cell reads as `expected`.
fn make(
    reading: &Arc<Reading>,
    kind: OpKind,
    at: usize,
    decide: Decide<'_>,
    expected: Option<&[String]>,
) -> Result<Option<Command>, EditError> {
    let store = &reading.edits;
    let overlay = store.overlay();
    let plan = plan(reading, &overlay, kind, at, decide)?;
    if let Some(expected) = expected {
        let now = cells_at(reading, &overlay, &plan.applied, at)?;
        if now.len() != expected.len() {
            return Err(EditError::ValueChanged { row: 0, column: at });
        }
        let rows = plan.applied.iter().flat_map(|run| run.clone());
        for ((now, expected), row) in now.iter().zip(expected).zip(rows) {
            if now != expected {
                let row = usize::try_from(row).unwrap_or(usize::MAX);
                return Err(EditError::ValueChanged { row, column: at });
            }
        }
    }
    let origin = matches!(plan.op.kind, OpKind::Delete).then(|| {
        Arc::new(TakenCells {
            reading: Arc::clone(reading),
            overlay: Arc::clone(&overlay),
        }) as Arc<dyn ColumnSource>
    });
    let before = Arc::clone(overlay.columns());
    drop(overlay);
    Ok(Some(commit(reading, plan, before, origin, None)))
}

/// Puts `values` back as a column at `at` in logical rows `rows`, by value:
/// a column delete undone after a save, or in a replay.
fn restore(
    reading: &Arc<Reading>,
    at: usize,
    rows: &[Range<u32>],
    values: Vec<String>,
) -> Result<Option<Command>, EditError> {
    let store = &reading.edits;
    let overlay = store.overlay();
    let map = overlay.map();
    let mut by_row = BTreeMap::new();
    let logical = rows.iter().flat_map(|run| run.clone());
    for (row, value) in logical.zip(&values) {
        let row = usize::try_from(row).unwrap_or(usize::MAX);
        let id = match map.slot(row) {
            Some(slot) => slot.id(),
            None if map.is_identity() && row < reading.index.row_count() => {
                RowId::original(u32::try_from(row).unwrap_or(u32::MAX))
            }
            None => return Err(EditError::NoSuchRow { row }),
        };
        by_row.insert(id, Arc::from(value.as_str()));
    }
    let kind = OpKind::Restore(Arc::new(by_row));
    let plan = plan(reading, &overlay, kind, at, Decide::Rows(rows))?;
    let before = Arc::clone(overlay.columns());
    drop(overlay);
    let restored = Some(Arc::from(values));
    Ok(Some(commit(reading, plan, before, None, restored)))
}

/// Makes `plan`, whose operations before were `before`, and gives its
/// command.
fn commit(
    reading: &Reading,
    plan: Plan,
    before: Arc<Columns>,
    origin: Option<Arc<dyn ColumnSource>>,
    restored: Option<Arc<[String]>>,
) -> Command {
    let store = &reading.edits;
    let edits = plan
        .rows
        .iter()
        .map(|(id, _, after)| (*id, after.clone()))
        .collect();
    let next_op = plan.op.id.0.saturating_add(1);
    store.change_columns(
        ColumnChange {
            columns: Arc::clone(&plan.after),
            edits,
        },
        next_op,
    );
    let at = plan.op.at;
    let inserts = plan.op.inserts();
    let column = Arc::new(Column {
        base: store.base(),
        op: plan.op,
        before,
        after: plan.after,
        rows: plan.rows,
        applied: plan.applied,
        origin,
        restored,
    });
    let edit = if inserts {
        Edit::InsertColumn { at, column }
    } else {
        Edit::DeleteColumn { at, column }
    };
    Command {
        lineage: store.lineage(),
        edit,
    }
}

/// Takes `column`'s operation out again, by identity: its undo in the
/// edits it was made in. The operations must be as it left them, and each
/// row it changed must read as it left it (an undo by value of a later
/// cell edit may have given a hatched cell another id).
fn take_out(reading: &Reading, column: &Column, edit: &Edit) -> Result<Option<Command>, EditError> {
    let store = &reading.edits;
    let overlay = store.overlay();
    let changed = || EditError::ValueChanged {
        row: 0,
        column: column.op.at,
    };
    if !same_columns(overlay.columns(), &column.after)
        || !rows_read_as(&overlay, &column.rows, &column.after, |(_, _, after)| after)
    {
        return Err(changed());
    }
    // No other row may have edited a cell the insert gave it: that edit
    // would be left behind, hidden.
    if column.op.inserts() {
        let cell = CellId::Inserted(column.op.id);
        let recorded = |id: RowId| column.rows.iter().any(|(row, _, _)| *row == id);
        if overlay
            .all()
            .any(|(id, edits)| edits.get(cell).is_some() && !recorded(id))
        {
            return Err(changed());
        }
    }
    drop(overlay);
    let edits = column
        .rows
        .iter()
        .map(|(id, before, _)| (*id, before.clone()))
        .collect();
    store.change_columns(
        ColumnChange {
            columns: Arc::clone(&column.before),
            edits,
        },
        0,
    );
    Ok(Some(Command {
        lineage: store.lineage(),
        edit: edit.clone(),
    }))
}

/// Makes `column`'s operation again, by identity: its redo in the edits it
/// was made in. The operations and the rows it changed must be as they
/// were before it.
fn put_back(reading: &Reading, column: &Column, edit: &Edit) -> Result<Option<Command>, EditError> {
    let store = &reading.edits;
    let overlay = store.overlay();
    if !same_columns(overlay.columns(), &column.before)
        || !rows_read_as(&overlay, &column.rows, &column.before, |(_, before, _)| {
            before
        })
    {
        return Err(EditError::ValueChanged {
            row: 0,
            column: column.op.at,
        });
    }
    drop(overlay);
    let edits = column
        .rows
        .iter()
        .map(|(id, _, after)| (*id, after.clone()))
        .collect();
    store.change_columns(
        ColumnChange {
            columns: Arc::clone(&column.after),
            edits,
        },
        column.op.id.0.saturating_add(1),
    );
    Ok(Some(Command {
        lineage: store.lineage(),
        edit: edit.clone(),
    }))
}

/// Whether the operations in effect are `expected`.
fn same_columns(now: &Arc<Columns>, expected: &Arc<Columns>) -> bool {
    Arc::ptr_eq(now, expected) || **now == **expected
}

/// Whether each recorded row's edits now read as the ones `pick` takes
/// from its record, under `columns`.
fn rows_read_as(
    overlay: &Overlay,
    rows: &[Changed],
    columns: &Columns,
    pick: impl Fn(&Changed) -> &Option<Arc<RowEdits>>,
) -> bool {
    rows.iter().all(|record| {
        let id = record.0;
        match (overlay.edits(id), pick(record)) {
            (None, None) => true,
            (Some(now), Some(expected)) => {
                Arc::ptr_eq(now, expected) || now.reads_as(expected, columns, id.inserted_index())
            }
            _ => false,
        }
    })
}

/// Works out the operation `kind` at `at` on the rows `decide` says, in
/// `overlay`: the rows it applies to, the rows whose edits change, and the
/// refusals (ADR-0004 decisions 5 and 8).
fn plan(
    reading: &Reading,
    overlay: &Overlay,
    kind: OpKind,
    at: usize,
    decide: Decide<'_>,
) -> Result<Plan, EditError> {
    let store = &reading.edits;
    let columns = overlay.columns();
    let counts = Counts::of(reading).ok_or(EditError::StillReading)?;
    let physical = reading.index.row_count();
    let op = ColumnOp {
        id: OpId(store.next_op()),
        at,
        kind,
        inserted_before: store.next_inserted(),
    };
    let mut walk = Walk {
        op: &op,
        columns,
        decide,
        cursor: 0,
        widest: 0,
        applied: Vec::new(),
        rows: Vec::new(),
        fresh: Vec::new(),
        misfit: None,
    };
    each_row(
        overlay,
        counts,
        physical,
        &mut |logical, id, shape| match shape {
            Shape::Unedited(own) => walk.unedited(logical, id, own),
            Shape::Edited(edits) => walk.edited(logical, id, edits),
        },
    );
    let widest = walk.widest;
    let past = if op.inserts() {
        at > widest
    } else {
        at >= widest
    };
    if past && matches!(decide, Decide::Rule) {
        return Err(EditError::NoSuchColumn { column: at });
    }
    if let Some(row) = walk.misfit {
        // By value, a row it applied to is too short for it now.
        return Err(EditError::ValueChanged { row, column: at });
    }
    if let Decide::Rows(runs) = decide
        && count(&walk.applied) != count(runs)
    {
        // By value, a row it applied to isn't there now.
        return Err(EditError::ValueChanged { row: 0, column: at });
    }
    let Walk {
        applied,
        mut rows,
        fresh,
        ..
    } = walk;
    // Rows the rule decided otherwise for than their default layouts, with
    // no edits yet: given a layout of their own (only by value).
    for (id, own, applies) in fresh {
        let mut ids = columns.fold(own).as_slice().into_owned();
        if applies {
            pad(&mut ids, &op, own.fields, &[]);
            op.apply(&mut ids);
        }
        let edits = new_edits(reading, id, own)?.with_layout(Some(Arc::from(ids)));
        rows.push((id, None, Some(Arc::new(edits))));
    }
    rows.sort_unstable_by_key(|(id, _, _)| *id);
    let after = Arc::new(columns.with(op.clone()));
    check_quote(reading, overlay, &rows, &after, at)?;
    Ok(Plan {
        op,
        after,
        rows,
        applied,
    })
}

/// How many rows `runs` hold.
fn count(runs: &[Range<u32>]) -> usize {
    runs.iter().map(|run| to_usize(run.end - run.start)).sum()
}

/// A logical row as the walk over the marks sees it.
enum Shape<'a> {
    /// No edits: its own cells, laid out by default.
    Unedited(EditOwn),
    /// Its edits.
    Edited(&'a Arc<RowEdits>),
}

/// Hands each logical row of `overlay` to `each`, in order, with its id
/// and shape: the file's rows with no edits by their field counts
/// (`physical` rows, all counted), so no row is read.
fn each_row<'a>(
    overlay: &'a Overlay,
    counts: Counts<'_>,
    physical: usize,
    each: &mut dyn FnMut(usize, RowId, Shape<'a>),
) {
    let map = overlay.map();
    let segments = match map.len() {
        Some(len) => map.segments(0..len),
        None => vec![Segment::Original(
            0..u32::try_from(physical).unwrap_or(u32::MAX),
        )],
    };
    let mut logical = 0usize;
    for segment in segments {
        match segment {
            Segment::Original(range) => {
                let rows = to_usize(range.start)..to_usize(range.end);
                let edited: Vec<(usize, &Arc<RowEdits>)> = overlay
                    .rows_in(rows.clone())
                    .filter_map(|(row, _)| Some((row, overlay.row_arc(row)?)))
                    .collect();
                let mut next_edited = edited.iter().peekable();
                let first = logical;
                counts.for_each_code(rows.clone(), &mut |row, code| {
                    let here = first + (row - rows.start);
                    if let Some(&(_, edits)) = next_edited.next_if(|(r, _)| *r == row) {
                        each(here, original_id(row), Shape::Edited(edits));
                    } else {
                        let blank = code.fields.is_none();
                        let own = EditOwn::original(code.fields.unwrap_or(1), blank);
                        each(here, original_id(row), Shape::Unedited(own));
                    }
                });
                logical += rows.len();
            }
            Segment::Inserted(range) => {
                for n in range {
                    let id = RowId::inserted(n);
                    match (overlay.edits(id), overlay.inserted(n)) {
                        (Some(edits), _) => each(logical, id, Shape::Edited(edits)),
                        (None, Some(row)) => {
                            let own = EditOwn::inserted(n, row.fields().len());
                            each(logical, id, Shape::Unedited(own));
                        }
                        (None, None) => {}
                    }
                    logical += 1;
                }
            }
        }
    }
}

/// The longest row of `overlay` now, in cells, from the field counts
/// (`None` until every row has one).
fn widest(reading: &Reading, overlay: &Overlay) -> Option<usize> {
    let counts = Counts::of(reading)?;
    let physical = reading.index.row_count();
    let columns = overlay.columns();
    let mut widest = 0;
    each_row(overlay, counts, physical, &mut |_, id, shape| {
        let len = match shape {
            Shape::Unedited(own) => columns.fold_len(own),
            Shape::Edited(edits) => edits.len_in(columns, id.inserted_index()),
        };
        widest = widest.max(len);
    });
    Some(widest)
}

/// [`plan`]'s walk over every logical row.
struct Walk<'a> {
    op: &'a ColumnOp,
    columns: &'a Columns,
    decide: Decide<'a>,
    /// Where `decide`'s rows have got to.
    cursor: usize,
    /// The longest row.
    widest: usize,
    /// The logical rows it applies to, as runs.
    applied: Vec<Range<u32>>,
    /// The rows whose edits change.
    rows: Vec<Changed>,
    /// Unedited rows that need a layout of their own: (row, its own cells,
    /// whether the operation applies).
    fresh: Vec<(RowId, EditOwn, bool)>,
    /// The first logical row `decide` names that is too short for the
    /// operation.
    misfit: Option<usize>,
}

impl Walk<'_> {
    /// Whether the operation applies to logical row `logical`, which rule 6
    /// says of `rule`.
    fn applies(&mut self, logical: usize, rule: bool) -> bool {
        match self.decide {
            Decide::Rule => rule,
            Decide::Rows(runs) => {
                let row = u32::try_from(logical).unwrap_or(u32::MAX);
                while self.cursor < runs.len() && runs[self.cursor].end <= row {
                    self.cursor += 1;
                }
                runs.get(self.cursor).is_some_and(|run| run.contains(&row))
            }
        }
    }

    /// Records logical row `logical`, `len` cells long and a blank line if
    /// `blank`, as applied to if `applies`. By value, a row too short for
    /// the operation now, or a blank line now (which gets no inserted
    /// cell), is a misfit; a restore pads a row too short for it instead.
    fn record(&mut self, logical: usize, len: usize, blank: bool, applies: bool) {
        if !applies {
            return;
        }
        let restores = matches!(self.op.kind, OpKind::Restore(_));
        if !restores && !self.op.applies(len, blank) && self.misfit.is_none() {
            self.misfit = Some(logical);
        }
        let row = u32::try_from(logical).unwrap_or(u32::MAX);
        match self.applied.last_mut() {
            Some(run) if run.end == row => run.end = row + 1,
            _ => self.applied.push(row..row + 1),
        }
    }

    /// A row with no edits, laid out by default.
    fn unedited(&mut self, logical: usize, id: RowId, own: EditOwn) {
        let len = self.columns.fold_len(own);
        self.widest = self.widest.max(len);
        let rule = self.op.applies(len, own.blank);
        let applies = self.applies(logical, rule);
        self.record(logical, len, own.blank, applies);
        if applies != rule {
            self.fresh.push((id, own, applies));
        }
    }

    /// A row with edits.
    fn edited(&mut self, logical: usize, id: RowId, edits: &Arc<RowEdits>) {
        let inserted = id.inserted_index();
        let layout = edits.layout_in(self.columns, inserted);
        let len = layout.len();
        self.widest = self.widest.max(len);
        let blank = edits.is_blank_in(self.columns, inserted);
        let rule = self.op.applies(len, blank);
        let applies = self.applies(logical, rule);
        self.record(logical, len, blank, applies);
        let by_default = edits.layout().is_none() && {
            let own = edits.own(inserted);
            applies == self.op.applies(self.columns.fold_len(own), own.blank)
        };
        if by_default || (edits.layout().is_some() && !applies) {
            return;
        }
        let mut ids = layout.ids().into_owned();
        if applies {
            pad(&mut ids, self.op, edits.fields(), edits.cells());
            self.op.apply(&mut ids);
        }
        trim(&mut ids, edits);
        let after = RowEdits::clone(edits).with_layout(Some(Arc::from(ids)));
        self.rows
            .push((id, Some(Arc::clone(edits)), Some(Arc::new(after))));
    }
}

/// Pads a row too short for cells put back by value (a column delete
/// undone after a save or in a replay, which had taken the hatched cell
/// the padding was before) with empty hatched cells up to the column.
fn pad(ids: &mut Vec<CellId>, op: &ColumnOp, fields: usize, cells: &[(CellId, Arc<str>)]) {
    if !matches!(op.kind, OpKind::Restore(_)) || ids.len() >= op.at {
        return;
    }
    let mut next = fresh_appended(fields, ids, cells);
    while ids.len() < op.at {
        ids.push(CellId::Appended(next));
        next += 1;
    }
}

/// Takes the hatched cells at the end of `ids` that hold nothing: padding
/// is only ever before an edited hatched cell (ADR-0005 decision 2), so a
/// delete of a row's last one gives the row its own bytes back (ADR-0014
/// decision 5).
fn trim(ids: &mut Vec<CellId>, edits: &RowEdits) {
    while let Some(&id @ CellId::Appended(_)) = ids.last() {
        if edits.get(id).is_some() {
            break;
        }
        ids.pop();
    }
}

/// Edits with no cell for row `id`, unedited so far, with `own` cells: for
/// a layout of its own. A row of the file is read, for its fields'
/// diagnostics.
fn new_edits(reading: &Reading, id: RowId, own: EditOwn) -> Result<RowEdits, EditError> {
    let Some(physical) = id.physical() else {
        return Ok(RowEdits::new(own.fields, Vec::new(), Arc::from([]), None));
    };
    let row = to_usize(physical);
    let Some(RowBytes { bytes, base, index }) =
        Document::row_bytes(reading, row).map_err(|error| EditError::Read { row, error })?
    else {
        return Err(EditError::NoSuchRow { row });
    };
    let parsed = reading
        .parser
        .parse_row_in(index, row, &bytes, base)
        .ok_or(EditError::NoSuchRow { row })?;
    let view = RowView::own(&reading.parser, &bytes, base, &parsed, id);
    let flagged = Arc::from(view.flagged_fields());
    Ok(RowEdits::new(parsed.fields().len(), Vec::new(), flagged, None).blank(own.blank))
}

/// ADR-0004 decision 8 after a column insert or delete (§7): the file's
/// open unterminated quote, if it is still in the document, must still be
/// the last cell of its row (the last row), so a cell inserted after it is
/// refused. Deleting it lifts the rule.
fn check_quote(
    reading: &Reading,
    overlay: &Overlay,
    rows: &[Changed],
    after: &Columns,
    at: usize,
) -> Result<(), EditError> {
    let Some(last) = open_quote_row(reading, overlay, &[]) else {
        return Ok(());
    };
    let Ok(logical) = overlay.map().logical_of(last) else {
        return Ok(());
    };
    let id = RowId::original(last);
    let edits = match rows.iter().find(|(row, _, _)| *row == id) {
        Some((_, _, after)) => after.clone(),
        None => overlay.edits(id).cloned(),
    };
    let fields = match &edits {
        Some(edits) => edits.fields(),
        None => Counts::of(reading)
            .and_then(|counts| counts.code_of(to_usize(last)))
            .and_then(|code| code.fields)
            .unwrap_or(1),
    };
    let Some(quote) = fields
        .checked_sub(1)
        .and_then(|q| u32::try_from(q).ok())
        .map(CellId::Field)
    else {
        return Ok(());
    };
    let ids: Cow<'_, [CellId]> = match &edits {
        Some(edits) => Cow::Owned(edits.layout_in(after, None).ids().into_owned()),
        None => Cow::Owned(
            after
                .fold(EditOwn::original(fields, false))
                .as_slice()
                .into_owned(),
        ),
    };
    match ids.iter().position(|&id| id == quote) {
        Some(p) if p + 1 != ids.len() => Err(EditError::AfterUnterminatedQuote {
            row: logical,
            column: at,
        }),
        _ => Ok(()),
    }
}

/// Cell `at` of each of logical rows `rows`, as it reads in `overlay`.
fn cells_at(
    reading: &Reading,
    overlay: &Overlay,
    rows: &[Range<u32>],
    at: usize,
) -> Result<Vec<String>, EditError> {
    let mut values = Vec::new();
    for run in rows {
        let range = to_usize(run.start)..to_usize(run.end);
        let start = range.start;
        let read = Document::read_rows_with(reading, overlay, range, |view| {
            view.value(at).map_or_else(String::new, Cow::into_owned)
        })
        .map_err(|error| EditError::Read { row: start, error })?;
        values.extend(read);
    }
    Ok(values)
}

/// A column delete's way back to the cells it took, as they read before
/// it: the reading it was made in, and its edits then (ADR-0014 decision
/// 3). It keeps that snapshot alive while the command is held.
struct TakenCells {
    reading: Arc<Reading>,
    overlay: Arc<Overlay>,
}

impl ColumnSource for TakenCells {
    fn values(&self, rows: &[Range<u32>], at: usize) -> Result<Vec<String>, ReadError> {
        let mut values = Vec::new();
        for run in rows {
            let range = to_usize(run.start)..to_usize(run.end);
            values.extend(Document::read_rows_with(
                &self.reading,
                &self.overlay,
                range,
                |view| view.value(at).map_or_else(String::new, Cow::into_owned),
            )?);
        }
        Ok(values)
    }
}

fn original_id(row: usize) -> RowId {
    RowId::original(u32::try_from(row).unwrap_or(u32::MAX))
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}
