//! Editing a document (task 2.1, DESIGN §3.6): cell edits, alone or in a
//! batch, the commands that undo, redo and replay them, and the full value
//! an editor starts from. See [`crate::edit`] for the overlay and the
//! command model.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, PoisonError};

use super::{Document, Reading, RowBytes, RowView};
use crate::diagnostics::Mark;
use crate::edit::{
    COLUMN_LIMIT, CellChange, Command, Edit, EditError, Lineage, Overlay, Replay, RowEdits,
};
use crate::index::Status;
use crate::rows::{ParsedRow, RowParser};
use crate::source::{ReadError, Storage};

/// One cell to change: where, and to what (`None`: missing, which only a
/// cell past the row's own fields can be). `expected` is the value a
/// command expects the cell to hold first; a new edit has none.
pub(super) struct Target<'a> {
    pub(super) row: usize,
    pub(super) column: usize,
    pub(super) value: Option<&'a str>,
    pub(super) expected: Option<Option<&'a str>>,
}

/// A row being changed: its bytes, as the file has it, and its edited
/// cells so far.
struct Work<'b> {
    bytes: Cow<'b, [u8]>,
    base: usize,
    parsed: ParsedRow,
    cells: Vec<(usize, Arc<str>)>,
    previous: Option<Arc<RowEdits>>,
    /// The row was read from the first 64 KB kept in memory.
    from_head: bool,
    /// The last cell of the row a target changed, for a refusal to name.
    last_column: usize,
}

impl Work<'_> {
    /// Cell `column`'s value now: `None` past the row's end.
    fn value(&self, parser: &RowParser, column: usize) -> Option<Cow<'_, str>> {
        if let Ok(at) = self.cells.binary_search_by_key(&column, |&(c, _)| c) {
            return Some(Cow::Borrowed(&self.cells[at].1));
        }
        if let Some(field) = self.parsed.field(column) {
            return Some(parser.display_value_in(&self.bytes, self.base, field));
        }
        let end = self.cells.last().map_or(0, |&(c, _)| c + 1);
        (column < end).then_some(Cow::Borrowed(""))
    }
}

impl Document {
    /// Sets cell (`row`, `column`) to `value`: the user committed an edit.
    /// `row` is the physical row (the header row, if any, is row 0, and can
    /// be edited too); `column` is a field of it, or a cell past its end (a
    /// hatched cell, ADR-0005 decision 2).
    ///
    /// Returns the [`Command`] that did it, for the app's undo history:
    /// undo applies its [`inverse`](Command::inverse), redo applies it
    /// again ([`apply`](Self::apply)). `None` if the cell already reads as
    /// `value`: committing an unchanged value is no edit (ADR-0008
    /// decision 3), and neither is `""` in a missing cell. Setting a cell
    /// back to its original display value removes its edit, so its original
    /// bytes come back (DESIGN §3.6); that is still a command, since the
    /// cell's value changed.
    ///
    /// It reads the row (one row, from the map or the copy), so it is fast
    /// enough for the main thread.
    ///
    /// # Errors
    ///
    /// [`EditError::NoSuchRow`] or [`EditError::NotReadYet`] for a row that
    /// can't be read now, [`EditError::TooFarRight`],
    /// [`EditError::AfterUnterminatedQuote`] (ADR-0004 decision 8), or
    /// [`EditError::Read`] if the row's bytes can't be read. The document
    /// is then unchanged.
    pub fn set_cell(
        &self,
        row: usize,
        column: usize,
        value: &str,
    ) -> Result<Option<Command>, EditError> {
        let target = Target {
            row,
            column,
            value: Some(value),
            expected: None,
        };
        let (lineage, mut changes) = self.change(None, &[target])?;
        Ok(changes.pop().map(|change| Command {
            lineage,
            edit: Edit::SetCell(change),
        }))
    }

    /// Sets several cells at once, in order, as one command (a paste, or
    /// clearing a selection): each `(row, column, value)` as
    /// [`set_cell`](Self::set_cell) would, but all of them or none. The
    /// whole batch is checked before anything changes, and each row's
    /// edits are rebuilt once. A cell may appear more than once: the last
    /// value wins. `None` if no cell changes.
    ///
    /// # Errors
    ///
    /// As for [`set_cell`](Self::set_cell), naming the first cell that
    /// can't be changed; nothing changes then.
    pub fn set_cells(&self, cells: &[(usize, usize, &str)]) -> Result<Option<Command>, EditError> {
        let targets: Vec<Target<'_>> = cells
            .iter()
            .map(|&(row, column, value)| Target {
                row,
                column,
                value: Some(value),
                expected: None,
            })
            .collect();
        let (lineage, changes) = self.change(None, &targets)?;
        let changed = !changes.is_empty();
        Ok(changed.then_some(Command {
            lineage,
            edit: Edit::SetCells(changes),
        }))
    }

    /// Applies `command`, if it still applies: redo applies a command
    /// again, undo applies its [`inverse`](Command::inverse). It applies
    /// only in the edits it was made in (its lineage), and only if every
    /// cell holds the command's old value and ends up holding its new one;
    /// otherwise nothing changes.
    ///
    /// # Errors
    ///
    /// [`EditError::OtherLineage`] for a command made before the file was
    /// read with another delimiter or encoding, [`EditError::ValueChanged`]
    /// if a cell doesn't hold what the command expects, and the errors of
    /// [`set_cell`](Self::set_cell).
    pub fn apply(&self, command: &Command) -> Result<(), EditError> {
        self.change(Some(command.lineage), &targets(command))
            .map(|_| ())
    }

    /// Applies `commands` in order, as [`apply`](Self::apply) does but
    /// whatever their lineage, and says which didn't apply and why: to
    /// recover a failed document's edits, by replaying the app's journal of
    /// commands into a freshly opened document of the same file (ADR-0008
    /// decision 5). A command that doesn't apply is skipped, and the rest
    /// are still tried; one that built on it then usually doesn't apply
    /// either, since its cell doesn't hold the value it expects.
    ///
    /// The commands that applied come back in this document's lineage, for
    /// the app's new undo history.
    ///
    /// Rows the index hasn't reached yet are refused with
    /// [`EditError::NotReadYet`], so call it once the index job has
    /// finished. It reads each row a command changes: call it off the main
    /// thread for a long history.
    pub fn replay(&self, commands: &[Command]) -> Replay {
        let mut replay = Replay::default();
        for (at, command) in commands.iter().enumerate() {
            match self.change(None, &targets(command)) {
                Ok((lineage, _)) => replay.commands.push(Command {
                    lineage,
                    edit: command.edit.clone(),
                }),
                Err(error) => replay.refused.push((at, error)),
            }
        }
        replay
    }

    /// Whether cell (`row`, `column`) can be edited now, for the app to
    /// decide before it opens the in-cell editor (task 2.5).
    ///
    /// # Errors
    ///
    /// What [`set_cell`](Self::set_cell) would refuse any value with: the
    /// row can't be read, the cell is too far right, or it is past an
    /// unterminated quote that is still open. (Setting the quote's own cell
    /// back to its original value while a cell after it is edited is
    /// refused only when it is committed.)
    pub fn can_edit(&self, row: usize, column: usize) -> Result<(), EditError> {
        let reading = self.current();
        let overlay = reading.edits.overlay();
        let work = Self::work(&reading, &overlay, row)?;
        check_column(&work, row, column)?;
        if let Some(quote) = open_quote(&work)
            && column > quote
        {
            return Err(EditError::AfterUnterminatedQuote { row, column });
        }
        Ok(())
    }

    /// The lineage of the commands made now (see [`Lineage`]).
    #[must_use]
    pub fn lineage(&self) -> Lineage {
        self.current().edits.lineage()
    }

    /// Whether the document has unsaved edits: some cell reads differently
    /// from the file. Edits set back to their original values don't count.
    #[must_use]
    pub fn has_edits(&self) -> bool {
        !self.current().edits.is_empty()
    }

    /// The edits' version: how many edits (each change, undo and redo)
    /// the current reading's edits have had. A save's
    /// [`snapshot_version`](crate::save::SaveProgress::snapshot_version)
    /// is one: the edits up to it are in the file the save writes, and
    /// those after it aren't. It counts within one set of edits; the
    /// reading a save makes starts its own, with the edits carried over.
    #[must_use]
    pub fn edit_version(&self) -> u64 {
        u64::try_from(self.current().edits.version()).unwrap_or(u64::MAX)
    }

    /// How many cells are edited.
    #[must_use]
    pub fn edited_cells(&self) -> usize {
        self.current().edits.cells()
    }

    /// Cell (`row`, `column`)'s whole display value as it reads now, for
    /// the in-cell editor and the inspector to start from (ADR-0008
    /// decision 3): never the grid's shortened text or its symbols. Empty
    /// for a cell past the end of its row (a hatched cell). `None` if the
    /// row can't be read yet. A value of many megabytes takes milliseconds:
    /// call it off the main thread if it may be long.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn full_value(&self, row: usize, column: usize) -> Result<Option<String>, ReadError> {
        let mut values = self.read_rows(row..row.saturating_add(1), |view| {
            view.value(column).map_or_else(String::new, Cow::into_owned)
        })?;
        Ok(values.pop())
    }

    /// The edited cells whose rows can't be trusted to be what they were
    /// edited from (task 1.9): rows read from the first 64 KB kept in
    /// memory, once those turn out to be a different version of the file
    /// than the copy ([`changed_on_disk`](Self::changed_on_disk)), whose
    /// bytes in the trusted copy differ or aren't there. Each as (row,
    /// column), in order. The edits are kept and still shown where the row
    /// can be read; the app names these cells (task 2.5) and so does Save
    /// As from an incomplete document (ADR-0008 decision 6). Empty while
    /// the first 64 KB can be trusted. It reads each edited row that came
    /// from them.
    ///
    /// A row the copy hasn't reached yet counts as conflicted until it
    /// has, if it ever does.
    #[must_use]
    pub fn edit_conflicts(&self) -> Vec<(usize, usize)> {
        let reading = self.current();
        if !reading.head_is_stale() {
            return Vec::new();
        }
        let overlay = reading.edits.overlay();
        let mut cells = Vec::new();
        for (row, edits) in overlay.all() {
            let Some(hash) = edits.head_hash() else {
                continue;
            };
            let same = matches!(
                Self::row_bytes(&reading, row),
                Ok(Some(RowBytes { bytes, .. })) if hash_of(&bytes) == hash
            );
            if !same {
                cells.extend(edits.cells().iter().map(|&(column, _)| (row, column)));
            }
        }
        cells
    }

    /// Applies `targets` in order, all or none, and returns the changes
    /// made (cells that already held their value are left out) with the
    /// lineage they were made in. With `lineage`, a command's, it must be
    /// the edits' own.
    fn change(
        &self,
        lineage: Option<Lineage>,
        targets: &[Target<'_>],
    ) -> Result<(Lineage, Vec<CellChange>), EditError> {
        // One change at a time, and none while the file is read again.
        let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let reading = self.current();
        Self::change_in(&reading, lineage, targets)
    }

    /// [`change`](Self::change) in `reading`, with the writer lock held by
    /// the caller: a save carries edits over to the reading of the file it
    /// wrote before it becomes the current one.
    pub(super) fn change_in(
        reading: &Reading,
        lineage: Option<Lineage>,
        targets: &[Target<'_>],
    ) -> Result<(Lineage, Vec<CellChange>), EditError> {
        let store = &reading.edits;
        if lineage.is_some_and(|lineage| lineage != store.lineage()) {
            return Err(EditError::OtherLineage);
        }
        let overlay = store.overlay();
        let parser = &reading.parser;
        // After a save, a missing cell a command names may be a field now
        // (see `holds`).
        let rebased = store.is_rebased();
        let mut rows: BTreeMap<usize, Work<'_>> = BTreeMap::new();
        let mut changes = Vec::new();
        for target in targets {
            let Target { row, column, .. } = *target;
            if let Entry::Vacant(entry) = rows.entry(row) {
                entry.insert(Self::work(reading, &overlay, row)?);
            }
            let Some(work) = rows.get_mut(&row) else {
                continue;
            };
            check_column(work, row, column)?;
            let old = work.value(parser, column).map(Cow::into_owned);
            if let Some(expected) = target.expected
                && !holds(old.as_deref(), expected, rebased)
            {
                return Err(EditError::ValueChanged { row, column });
            }
            set(work, parser, row, column, target.value, rebased)?;
            work.last_column = column;
            let new = work.value(parser, column).map(Cow::into_owned);
            if target.expected.is_some() && !holds(new.as_deref(), target.value, rebased) {
                // The command doesn't fit the row as it is: a missing cell
                // it means can't be, or one it means to empty would go.
                return Err(EditError::ValueChanged { row, column });
            }
            if old != new {
                changes.push(CellChange {
                    row,
                    column,
                    old,
                    new,
                });
            }
        }

        // A batch that leaves every cell as it found it (x, then back) is no
        // edit at all: each cell's first change's old value against its last
        // change's new one, by their places in `changes`.
        let mut net: BTreeMap<(usize, usize), (usize, usize)> = BTreeMap::new();
        for (at, change) in changes.iter().enumerate() {
            net.entry((change.row, change.column))
                .and_modify(|(_, last)| *last = at)
                .or_insert((at, at));
        }
        if net
            .values()
            .all(|&(first, last)| changes[first].old == changes[last].new)
        {
            changes.clear();
        }

        // ADR-0004 decision 8, on each row as it ends up.
        for (&row, work) in &rows {
            if let Some(quote) = open_quote(work)
                && work.cells.last().is_some_and(|&(c, _)| c > quote)
            {
                let column = work.last_column;
                return Err(EditError::AfterUnterminatedQuote { row, column });
            }
        }

        let changed: std::collections::BTreeSet<usize> =
            changes.iter().map(|change| change.row).collect();
        let updates = rows
            .into_iter()
            .filter(|(row, _)| changed.contains(row))
            .map(|(row, work)| (row, row_edits(reading, parser, row, work)))
            .collect();
        // Let go of this snapshot first: the store copies the overlay if
        // anyone else holds it, and this edit mustn't count.
        drop(overlay);
        if !changes.is_empty() {
            store.set_rows(updates);
        }
        Ok((store.lineage(), changes))
    }

    /// Row `row`, read for a change.
    fn work<'b>(
        reading: &'b Reading,
        overlay: &Overlay,
        row: usize,
    ) -> Result<Work<'b>, EditError> {
        // As `Reading::rows_from` decides it.
        let from_head = !reading.head_is_stale() && reading.index.row_count() < reading.head_rows;
        let Some(RowBytes { bytes, base, index }) =
            Self::row_bytes(reading, row).map_err(|error| EditError::Read { row, error })?
        else {
            return Err(missing_row(reading, row));
        };
        let Some(parsed) = reading.parser.parse_row_in(index, row, &bytes, base) else {
            return Err(missing_row(reading, row));
        };
        let previous = overlay.row_arc(row).cloned();
        let cells = previous
            .as_ref()
            .map(|edits| edits.cells().to_vec())
            .unwrap_or_default();
        Ok(Work {
            bytes,
            base,
            parsed,
            cells,
            previous,
            from_head,
            last_column: 0,
        })
    }
}

/// The targets of `command`'s cells: each from its old value to its new.
fn targets(command: &Command) -> Vec<Target<'_>> {
    command
        .changes()
        .iter()
        .map(|change| Target {
            row: change.row,
            column: change.column,
            value: change.new.as_deref(),
            expected: Some(change.old.as_deref()),
        })
        .collect()
}

/// The column limit: a cell past the row's fields and past
/// [`COLUMN_LIMIT`] can't be edited.
fn check_column(work: &Work<'_>, row: usize, column: usize) -> Result<(), EditError> {
    if column >= work.parsed.fields().len().max(COLUMN_LIMIT) {
        return Err(EditError::TooFarRight { row, column });
    }
    Ok(())
}

/// The column of the row's unterminated quote, if it has one and it is
/// still open (unedited). It is always the row's last field.
fn open_quote(work: &Work<'_>) -> Option<usize> {
    let fields = work.parsed.fields();
    let quote = fields.len().checked_sub(1)?;
    let open = fields[quote].unterminated()
        && work
            .cells
            .binary_search_by_key(&quote, |&(c, _)| c)
            .is_err();
    open.then_some(quote)
}

/// Whether a cell that reads as `actual` holds what a command means by
/// `meant`: the same value, or, once the document has been saved and
/// rebased onto the file it wrote (`rebased`, task 2.2), an empty field
/// where the command means a missing cell. Saving a hatched cell's edit
/// (ADR-0005 decision 2) makes it, and any padding before it, a field of
/// the file; the command still says "missing". So undoing a hatched edit
/// after a save empties the cell rather than shortening the row, which no
/// cell edit can do, and redoing it then finds the empty field it left.
fn holds(actual: Option<&str>, meant: Option<&str>, rebased: bool) -> bool {
    actual == meant || (rebased && meant.is_none() && actual == Some(""))
}

/// Sets `work`'s cell `column` to `value`. Back to the original display
/// value (`""`, or missing, past the row's fields) removes the edit. Only
/// a cell past the row's fields can be made missing; once the document is
/// `rebased` (see [`holds`]), making one of its fields missing empties it.
fn set(
    work: &mut Work<'_>,
    parser: &RowParser,
    row: usize,
    column: usize,
    value: Option<&str>,
    rebased: bool,
) -> Result<(), EditError> {
    let value = match value {
        None if rebased && work.parsed.field(column).is_some() => Some(""),
        value => value,
    };
    let original = work
        .parsed
        .field(column)
        .map(|field| parser.display_value_in(&work.bytes, work.base, field));
    let back = match (&original, value) {
        (Some(original), Some(value)) => original == value,
        (Some(_), None) => return Err(EditError::ValueChanged { row, column }),
        (None, Some(value)) => value.is_empty(),
        (None, None) => true,
    };
    match (work.cells.binary_search_by_key(&column, |&(c, _)| c), back) {
        (Ok(at), true) => {
            work.cells.remove(at);
        }
        (Ok(at), false) => work.cells[at].1 = Arc::from(value.unwrap_or_default()),
        (Err(_), true) => {}
        (Err(at), false) => work
            .cells
            .insert(at, (column, Arc::from(value.unwrap_or_default()))),
    }
    Ok(())
}

/// A changed row's new edits, or `None` if it has none left.
fn row_edits(
    reading: &Reading,
    parser: &RowParser,
    row: usize,
    work: Work<'_>,
) -> Option<RowEdits> {
    if work.cells.is_empty() {
        return None;
    }
    let fields = work.parsed.fields().len();
    // The row's own fields' diagnostics, worked out the first time it is
    // edited: none to look for if its marks say it has none.
    let flagged = match &work.previous {
        Some(previous) => Arc::clone(previous.flagged()),
        None => {
            let unflagged = reading.diagnostics.get().is_some_and(|diagnostics| {
                row < diagnostics.marked_rows_and_mode().0
                    && !diagnostics.row_is(row, Mark::Flagged)
            });
            if unflagged {
                Arc::from([])
            } else {
                let view = RowView::new(parser, &work.bytes, work.base, &work.parsed, None);
                Arc::from(view.flagged_fields())
            }
        }
    };
    let head_hash = match &work.previous {
        Some(previous) => previous.head_hash(),
        None => work.from_head.then(|| hash_of(&work.bytes)),
    };
    Some(RowEdits::new(fields, work.cells, flagged, head_hash))
}

/// Why row `row` can't be edited: it isn't there (the index is complete
/// without it); it can't be read, because the file changed while it was
/// read or was deleted on its share, so the row will never come; or it
/// hasn't been read yet (the index hasn't reached it, or stopped before it:
/// its drive may come back).
fn missing_row(reading: &Reading, row: usize) -> EditError {
    if reading.index.status() == Status::Complete {
        EditError::NoSuchRow { row }
    } else if reading.source.changed_on_disk() {
        EditError::Read {
            row,
            error: ReadError::changed_on_disk(),
        }
    } else if reading.source.storage() == Storage::Deleted {
        EditError::Read {
            row,
            error: ReadError::already_deleted(),
        }
    } else {
        EditError::NotReadYet { row }
    }
}

/// A row's bytes, hashed, to tell later whether the trusted copy has the
/// same row.
fn hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}
