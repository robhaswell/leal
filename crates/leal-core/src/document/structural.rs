//! Inserting and deleting rows (task 2.4a, `docs/tasks/2.4.md` §1, §4 and
//! §7): the commands, their undo and redo by identity within the same
//! edits, and by value in other edits (after a save, or in a replay).
//!
//! Row inserts and deletes need the whole file read and trusted, and no
//! save running (ADR-0014 decision 1), so the piece list always covers a
//! fixed row count: the index's, complete.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use super::{Document, Reading};
use crate::edit::{
    CellId, Command, Edit, EditError, InsertedRow, Lineage, Overlay, Own as EditOwn, Piece,
    RowChange, RowEdits, RowId, RowMap, RowSource, Rows, Value,
};
use crate::index::Status;
use crate::rows::RowParser;
use crate::source::{ReadError, Storage};

impl Document {
    /// Inserts `rows` (each a row's values) before logical row `at` (the
    /// row count appends them), as one command, for the app's undo history
    /// (undo deletes them again). An empty row has one empty cell, as a
    /// blank line reads. Row 0 is the header row when the file has one: a
    /// row inserted there becomes the header (`docs/tasks/2.4.md` §7).
    /// `None` if `rows` is empty.
    ///
    /// Fast enough for the main thread: it touches one leaf of the piece
    /// list, whatever the file's size.
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] until the whole file has been read,
    /// [`EditError::Saving`] while a save runs (ADR-0014 decision 1),
    /// [`EditError::NoSuchRow`] past the end, and
    /// [`EditError::AfterUnterminatedQuote`] after the row of an
    /// unterminated quote that is still open (ADR-0004 decision 8). The
    /// document is then unchanged.
    pub fn insert_rows(
        &self,
        at: usize,
        rows: &[Vec<String>],
    ) -> Result<Option<Command>, EditError> {
        let rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| Value::from(value.as_str()))
                    .collect()
            })
            .collect();
        self.change_rows(|reading| insert_new(reading, at, &rows, false))
    }

    /// Deletes logical rows `at..at + count`, as one command (undo puts
    /// back the same rows, with their edits and original bytes). Deleting
    /// the header row makes the next row the header. `None` if `count` is
    /// 0.
    ///
    /// # Errors
    ///
    /// As for [`insert_rows`](Self::insert_rows): [`EditError::NoSuchRow`]
    /// if a row isn't there.
    pub fn delete_rows(&self, at: usize, count: usize) -> Result<Option<Command>, EditError> {
        self.change_rows(|reading| delete_at(reading, at, count, None))
    }

    /// Whether rows can be inserted or deleted now, for the app to enable
    /// **Insert Row** and **Delete Row** (task 2.5a).
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] or [`EditError::Saving`].
    pub fn can_change_rows(&self) -> Result<(), EditError> {
        if self.saving.load(Ordering::Acquire) {
            return Err(EditError::Saving);
        }
        whole_file(&self.current())
    }

    /// Whether rows can be inserted before logical row `at` now.
    ///
    /// # Errors
    ///
    /// As for [`can_change_rows`](Self::can_change_rows), and
    /// [`EditError::NoSuchRow`] or [`EditError::AfterUnterminatedQuote`] as
    /// [`insert_rows`](Self::insert_rows) would refuse them.
    pub fn can_insert_rows(&self, at: usize) -> Result<(), EditError> {
        self.can_change_rows()?;
        let reading = self.current();
        let overlay = reading.edits.overlay();
        let map = begun(&reading, &overlay);
        if at > map.len().unwrap_or(0) {
            return Err(EditError::NoSuchRow { row: at });
        }
        if let Some(quote) = open_quote_row(&reading, &overlay, &[])
            && let Ok(logical) = map.logical_of(quote)
            && at > logical
        {
            return Err(EditError::AfterUnterminatedQuote { row: at, column: 0 });
        }
        Ok(())
    }

    /// Applies a row insert or delete command (`edit`), as
    /// [`apply`](Self::apply) (with its `lineage`) or
    /// [`replay`](Self::replay) (with none) do. Returns the command as it
    /// applied: the same rows, in the edits it was made in, or new rows of
    /// this document's, by value.
    pub(super) fn apply_rows(
        &self,
        lineage: Option<Lineage>,
        edit: &Edit,
    ) -> Result<Option<Command>, EditError> {
        self.change_rows(|reading| {
            let store = &reading.edits;
            if lineage.is_some_and(|lineage| lineage != store.lineage()) {
                return Err(EditError::OtherLineage);
            }
            let (insert, at, rows) = match edit {
                Edit::InsertRows { at, rows } => (true, *at, rows),
                Edit::DeleteRows { at, rows } => (false, *at, rows),
                Edit::SetCell(_)
                | Edit::SetCells(_)
                | Edit::InsertColumn { .. }
                | Edit::DeleteColumn { .. } => return Ok(None),
            };
            if rows.base == store.base() {
                // The same rows, by identity: their original bytes come
                // back with them.
                return if insert {
                    restore(reading, at, rows)
                } else {
                    delete_at(reading, at, rows.len(), Some(rows))
                };
            }
            // Other edits (ADR-0014 decision 3): by value.
            let values = rows
                .values(&reading.parser)
                .map_err(|error| EditError::Read { row: at, error })?;
            if values.len() != rows.len() {
                return Err(EditError::ValueChanged { row: at, column: 0 });
            }
            if insert {
                return insert_new(reading, at, &values, true);
            }
            let now = values_of(reading, at..at + rows.len())?;
            for (i, expected) in values.iter().enumerate() {
                let expected: Vec<String> = expected
                    .iter()
                    .map(|value| value.text().to_owned())
                    .collect();
                let Some(row) = now.get(i) else {
                    return Err(EditError::NoSuchRow { row: at + i });
                };
                // Empty cells at the end don't count: undoing an edit past a
                // row's end after a save leaves an empty field there
                // (ADR-0012 decision 4), which reads the same.
                let (row, expected) = (filled(row), filled(&expected));
                if row != expected {
                    let column = row
                        .iter()
                        .zip(expected)
                        .position(|(a, b)| a != b)
                        .unwrap_or_else(|| row.len().min(expected.len()));
                    return Err(EditError::ValueChanged {
                        row: at + i,
                        column,
                    });
                }
            }
            delete_at(reading, at, rows.len(), None)
        })
    }

    /// Runs `change` on the current reading, one change at a time, once
    /// rows may be inserted or deleted.
    pub(super) fn change_rows(
        &self,
        change: impl FnOnce(&Arc<Reading>) -> Result<Option<Command>, EditError>,
    ) -> Result<Option<Command>, EditError> {
        let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        // Under the writer lock: a save takes its snapshot under it, after
        // saying it runs.
        if self.saving.load(Ordering::Acquire) {
            return Err(EditError::Saving);
        }
        let reading = self.current();
        whole_file(&reading)?;
        change(&reading)
    }
}

/// `values` without the empty cells at the end.
fn filled(values: &[String]) -> &[String] {
    let end = values
        .iter()
        .rposition(|value| !value.is_empty())
        .map_or(0, |last| last + 1);
    &values[..end]
}

/// [`EditError::StillReading`] unless the whole file has been read, and
/// copied if it is on a removable drive or a share, and can be trusted
/// (ADR-0014 decision 1).
pub(super) fn whole_file(reading: &Reading) -> Result<(), EditError> {
    let complete = reading.index.status() == Status::Complete
        && reading.source.storage() != Storage::Reading
        && !reading.head_is_stale();
    if complete {
        Ok(())
    } else {
        Err(EditError::StillReading)
    }
}

/// The file's row count, which the piece list covers once the index is
/// complete.
fn physical_rows(reading: &Reading) -> u32 {
    u32::try_from(reading.index.row_count()).unwrap_or(u32::MAX)
}

/// The piece list now, tracking rows (not the identity).
fn begun(reading: &Reading, overlay: &Overlay) -> RowMap {
    let mut map = overlay.map().clone();
    map.begin(physical_rows(reading));
    map
}

/// Inserts new rows of `values` at `at`. A row of no values has one empty
/// cell, as a blank line reads, unless `exact` (rows put back by value,
/// which column deletes may have left with none, task 2.4b).
fn insert_new(
    reading: &Arc<Reading>,
    at: usize,
    values: &[Vec<Value>],
    exact: bool,
) -> Result<Option<Command>, EditError> {
    if values.is_empty() {
        return Ok(None);
    }
    let store = &reading.edits;
    let overlay = store.overlay();
    let mut map = begun(reading, &overlay);
    if at > map.len().unwrap_or(0) {
        return Err(EditError::NoSuchRow { row: at });
    }
    let gap = u32::try_from(map.physical_at_or_after(at)).unwrap_or(u32::MAX);
    let first = store.next_inserted();
    let count = u32::try_from(values.len()).unwrap_or(u32::MAX);
    let next = first
        .checked_add(count)
        .ok_or(EditError::NoSuchRow { row: at })?;
    let piece = Piece::Inserted {
        gap,
        first,
        len: count,
    };
    if !map.insert(at, &[piece]) {
        return Err(EditError::ValueChanged { row: at, column: 0 });
    }
    check_quote(reading, &overlay, &map, &[], at)?;
    let inserted: Vec<(u32, Arc<InsertedRow>)> = (first..next)
        .zip(values)
        .map(|(n, values)| {
            let row = if exact {
                InsertedRow::exactly(gap, values)
            } else {
                InsertedRow::new(gap, values)
            };
            (n, Arc::new(row))
        })
        .collect();
    drop(overlay);
    store.change_rows(RowChange {
        map,
        edits: Vec::new(),
        inserted: inserted
            .iter()
            .map(|(n, row)| (*n, Some(Arc::clone(row))))
            .collect(),
        touched: vec![piece.ids()],
        next_inserted: next,
    });
    Ok(Some(Command {
        lineage: store.lineage(),
        edit: Edit::InsertRows {
            at,
            rows: Arc::new(Rows {
                base: store.base(),
                pieces: vec![piece],
                count: values.len(),
                inserted,
                edits: Vec::new(),
                origin: None,
                columns: store.columns(),
            }),
        },
    }))
}

/// Deletes rows `at..at + count`. With `expected`, a delete command's (or
/// an insert's, undone) rows in the same edits, they must be the rows
/// there, with the same edits.
fn delete_at(
    reading: &Arc<Reading>,
    at: usize,
    count: usize,
    expected: Option<&Rows>,
) -> Result<Option<Command>, EditError> {
    if count == 0 {
        return Ok(None);
    }
    let store = &reading.edits;
    let overlay = store.overlay();
    let mut map = begun(reading, &overlay);
    let len = map.len().unwrap_or(0);
    if at.saturating_add(count) > len {
        return Err(EditError::NoSuchRow { row: at.max(len) });
    }
    let removed = map.remove(at..at + count);
    // What the rows take with them: their edits, and the inserted rows.
    let mut edits: Vec<(RowId, Arc<RowEdits>)> = Vec::new();
    let mut inserted: Vec<(u32, Arc<InsertedRow>)> = Vec::new();
    for &piece in &removed {
        edits.extend(
            overlay
                .edits_in(piece.ids())
                .map(|(id, row)| (id, Arc::clone(row))),
        );
        if let Piece::Inserted { first, len, .. } = piece {
            for n in first..first + len {
                if let Some(row) = overlay.inserted(n) {
                    inserted.push((n, Arc::clone(row)));
                }
            }
        }
    }
    if let Some(expected) = expected {
        let same = removed == expected.pieces
            && same_rows(&edits, &expected.edits)
            && same_rows(&inserted, &expected.inserted);
        if !same {
            return Err(EditError::ValueChanged { row: at, column: 0 });
        }
    }
    check_quote(reading, &overlay, &map, &[], at)?;
    let original = removed
        .iter()
        .any(|piece| matches!(piece, Piece::Original { .. }));
    let origin = original.then(|| Arc::new(BaseRows(Arc::clone(reading))) as Arc<dyn RowSource>);
    let change = RowChange {
        map,
        edits: edits.iter().map(|(id, _)| (*id, None)).collect(),
        inserted: inserted.iter().map(|(n, _)| (*n, None)).collect(),
        touched: removed.iter().map(|piece| piece.ids()).collect(),
        next_inserted: store.next_inserted(),
    };
    drop(overlay);
    store.change_rows(change);
    Ok(Some(Command {
        lineage: store.lineage(),
        edit: Edit::DeleteRows {
            at,
            rows: Arc::new(Rows {
                base: store.base(),
                pieces: removed,
                count,
                inserted,
                edits,
                origin,
                columns: store.columns(),
            }),
        },
    }))
}

/// Puts `rows` (a delete's, undone, or an insert's, redone) back at `at`,
/// by identity, with their edits.
fn restore(reading: &Arc<Reading>, at: usize, rows: &Rows) -> Result<Option<Command>, EditError> {
    let store = &reading.edits;
    let overlay = store.overlay();
    let mut map = begun(reading, &overlay);
    if at > map.len().unwrap_or(0) {
        return Err(EditError::NoSuchRow { row: at });
    }
    let changed = EditError::ValueChanged { row: at, column: 0 };
    // The rows must not be in the document now.
    for &piece in &rows.pieces {
        let present = match piece {
            Piece::Original { start, len } => !map.originals_in(start..start + len).is_empty(),
            Piece::Inserted { first, len, .. } => {
                (first..first + len).any(|n| overlay.inserted(n).is_some())
            }
        };
        if present {
            return Err(changed);
        }
    }
    if !map.insert(at, &rows.pieces) {
        return Err(changed);
    }
    check_quote(reading, &overlay, &map, &rows.edits, at)?;
    let change = RowChange {
        map,
        edits: rows
            .edits
            .iter()
            .map(|(id, edits)| (*id, Some(Arc::clone(edits))))
            .collect(),
        inserted: rows
            .inserted
            .iter()
            .map(|(n, row)| (*n, Some(Arc::clone(row))))
            .collect(),
        touched: rows.ids().collect(),
        next_inserted: store.next_inserted(),
    };
    drop(overlay);
    store.change_rows(change);
    Ok(Some(Command {
        lineage: store.lineage(),
        edit: Edit::InsertRows {
            at,
            rows: Arc::new(Rows {
                base: rows.base,
                pieces: rows.pieces.clone(),
                count: rows.count,
                inserted: rows.inserted.clone(),
                edits: rows.edits.clone(),
                origin: rows.origin.clone(),
                columns: Arc::clone(&rows.columns),
            }),
        },
    }))
}

/// Whether two lists of shared values are the same, in order.
fn same_rows<K: PartialEq, V: PartialEq>(a: &[(K, Arc<V>)], b: &[(K, Arc<V>)]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| a.0 == b.0 && (Arc::ptr_eq(&a.1, &b.1) || a.1 == b.1))
}

/// The physical row of the file's unterminated quote, if it has one and it
/// is still open (its field unedited), with `restored` edits (coming back
/// with rows) taken first.
pub(super) fn open_quote_row(
    reading: &Reading,
    overlay: &Overlay,
    restored: &[(RowId, Arc<RowEdits>)],
) -> Option<u32> {
    reading.index.unterminated_quote()?;
    let last = physical_rows(reading).checked_sub(1)?;
    let id = RowId::original(last);
    let edits = restored
        .iter()
        .find(|(row, _)| *row == id)
        .map(|(_, edits)| edits)
        .or_else(|| overlay.edits(id));
    // The quote is the row's last field, unless it is edited or a column
    // delete took it (task 2.4b).
    let columns = overlay.columns();
    let open = match edits {
        Some(edits) => {
            let quote = last_field(edits.fields())?;
            edits.get(quote).is_none()
                && (columns.is_empty() || edits.layout_in(columns, None).ids().contains(&quote))
        }
        None => {
            columns.is_empty() || {
                let code = super::columns::Counts::of(reading)
                    .and_then(|counts| counts.code_of(to_usize(last)));
                match code.and_then(|code| code.fields) {
                    Some(fields) => {
                        let quote = last_field(fields)?;
                        let own = EditOwn::original(fields, false);
                        columns.fold(own).as_slice().contains(&quote)
                    }
                    None => true,
                }
            }
        }
    };
    open.then_some(last)
}

/// The id of the last of `fields` fields.
fn last_field(fields: usize) -> Option<CellId> {
    let last = fields.checked_sub(1)?;
    Some(CellId::Field(u32::try_from(last).ok()?))
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// ADR-0004 decision 8 after a row insert or delete (§7): an open
/// unterminated quote's row, if it is still in the document, must be the
/// last row, or the rows after it would be written inside the quote.
/// O(log P).
fn check_quote(
    reading: &Reading,
    overlay: &Overlay,
    map: &RowMap,
    restored: &[(RowId, Arc<RowEdits>)],
    row: usize,
) -> Result<(), EditError> {
    let Some(quote) = open_quote_row(reading, overlay, restored) else {
        return Ok(());
    };
    match (map.logical_of(quote), map.len()) {
        (Ok(logical), Some(len)) if logical + 1 != len => {
            Err(EditError::AfterUnterminatedQuote { row, column: 0 })
        }
        _ => Ok(()),
    }
}

/// Logical rows `rows` as they read now, each as its cells' values.
fn values_of(reading: &Reading, rows: Range<usize>) -> Result<Vec<Vec<String>>, EditError> {
    let row = rows.start;
    Document::read_rows_of(reading, rows, |view| row_values(&view))
        .map_err(|error| EditError::Read { row, error })
}

/// A row's cells' values, as it reads.
fn row_values(view: &super::RowView<'_>) -> Vec<String> {
    (0..view.len())
        .map(|column| view.value(column).map_or_else(String::new, Cow::into_owned))
        .collect()
}

/// A delete command's way back to its original rows' values: the reading
/// it was made in (ADR-0014 decision 3).
struct BaseRows(Arc<Reading>);

impl RowSource for BaseRows {
    fn values(
        &self,
        rows: Range<u32>,
        edits: &Overlay,
        parser: &RowParser,
    ) -> Result<Vec<Vec<Value>>, ReadError> {
        let rows = usize::try_from(rows.start).unwrap_or(usize::MAX)
            ..usize::try_from(rows.end).unwrap_or(usize::MAX);
        let mut values = Vec::with_capacity(rows.len());
        // Unedited fields as their bytes (task 2.4c).
        Document::each_physical(&self.0, edits, rows, &mut |view| {
            let layout = view.layout();
            values.push(
                (0..layout.len())
                    .map(|column| match view.cell_in(&layout, column) {
                        Some((_, cell)) => view.put_back(cell, parser),
                        None => Value::from(""),
                    })
                    .collect(),
            );
        })?;
        Ok(values)
    }
}
