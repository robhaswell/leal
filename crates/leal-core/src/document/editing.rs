//! Editing a document (task 2.1, DESIGN §3.6): cell edits, alone or in a
//! batch, the commands that undo, redo and replay them, and the full value
//! an editor starts from. See [`crate::edit`] for the overlay and the
//! command model, and `structural.rs` for row inserts and deletes.
//!
//! Rows are logical rows (task 2.4a): the piece list says which row of the
//! file, or which inserted row, each one is, and a row's edits are kept by
//! its [`RowId`](crate::edit::RowId).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, PoisonError};

use super::{Document, Reading, RowBytes, RowView};
use crate::diagnostics::Mark;
use crate::edit::{
    COLUMN_LIMIT, CellChange, CellId, Columns, Command, Edit, EditError, InsertedRow, Layout,
    Lineage, Overlay, Own as EditOwn, Parts, Replay, RowEdits, RowId, Slot, fresh_appended,
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

/// A row's own cells, for a change: its bytes as the file has it, or an
/// inserted row's values.
enum Own<'b> {
    File {
        bytes: Cow<'b, [u8]>,
        base: usize,
        parsed: ParsedRow,
        /// Its physical row.
        row: usize,
        /// The row was read from the first 64 KB kept in memory.
        from_head: bool,
    },
    New(Arc<InsertedRow>),
}

/// A row being changed: its own cells, and its edited cells and layout so
/// far (task 2.4b: cells by identity, `edit::columns`).
struct Work<'b> {
    own: Own<'b>,
    id: RowId,
    /// The own field count its edits are made against: the row's as read
    /// when first edited.
    fields: usize,
    /// It is a blank line of the file.
    blank: bool,
    cells: Vec<(CellId, Arc<str>)>,
    /// Its layout, if written out.
    layout: Option<Vec<CellId>>,
    columns: Arc<Columns>,
    previous: Option<Arc<RowEdits>>,
    /// The last cell of the row a target changed, for a refusal to name.
    last_column: usize,
}

impl<'b> Work<'b> {
    /// Row `id`, with `own` cells (a blank line of the file if `blank`),
    /// and its edits so far, `previous`, under `overlay`'s columns.
    fn of(
        own: Own<'b>,
        id: RowId,
        blank: bool,
        previous: Option<Arc<RowEdits>>,
        overlay: &Overlay,
    ) -> Work<'b> {
        let own_len = match &own {
            Own::File { parsed, .. } => parsed.fields().len(),
            Own::New(row) => row.fields().len(),
        };
        let (fields, blank, cells, layout) = match &previous {
            Some(edits) => (
                edits.fields(),
                edits.is_blank(),
                edits.cells().to_vec(),
                edits.layout().map(<[CellId]>::to_vec),
            ),
            None => (own_len, blank, Vec::new(), None),
        };
        Work {
            own,
            id,
            fields,
            blank,
            cells,
            layout,
            columns: Arc::clone(overlay.columns()),
            previous,
            last_column: 0,
        }
    }
}

impl Work<'_> {
    /// How many cells of its own the row has, as read.
    fn own_len(&self) -> usize {
        match &self.own {
            Own::File { parsed, .. } => parsed.fields().len(),
            Own::New(row) => row.fields().len(),
        }
    }

    /// Its own cell `k`'s value, unedited, if it has one.
    fn own_value(&self, parser: &RowParser, k: usize) -> Option<Cow<'_, str>> {
        match &self.own {
            Own::File {
                bytes,
                base,
                parsed,
                ..
            } => parsed
                .field(k)
                .map(|field| parser.display_value_in(bytes, *base, field)),
            Own::New(row) => row.fields().get(k).map(|v| Cow::Borrowed(v.as_ref())),
        }
    }

    /// What the default layout needs to know of the row as read.
    fn shape(&self) -> EditOwn {
        match &self.own {
            Own::File { .. } => EditOwn::original(self.own_len(), self.blank),
            Own::New(_) => EditOwn::inserted(self.id.inserted_index().unwrap_or(0), self.own_len()),
        }
    }

    /// Which cell each logical column is now.
    fn layout(&self) -> Layout<'_> {
        let parts = Parts {
            fields: self.fields,
            blank: self.blank,
            cells: &self.cells,
            layout: self.layout.as_deref(),
        };
        Layout::of_parts(&self.columns, self.shape(), Some(parts))
    }

    /// Cell `id`'s value unedited: `None` for a hatched cell, which is
    /// missing.
    fn original(&self, parser: &RowParser, id: CellId) -> Option<Cow<'_, str>> {
        match id {
            CellId::Field(k) | CellId::Appended(k) => {
                self.own_value(parser, usize::try_from(k).ok()?)
            }
            CellId::Inserted(op) => self
                .columns
                .inserted_value(op, self.id)
                .map(|value| Cow::Owned(value.to_owned())),
        }
    }

    /// Cell `column`'s value now: `None` past the row's end.
    fn value(&self, parser: &RowParser, column: usize) -> Option<Cow<'_, str>> {
        let id = self.layout().get(column)?;
        if let Ok(at) = self.cells.binary_search_by_key(&id, |&(c, _)| c) {
            return Some(Cow::Borrowed(&self.cells[at].1));
        }
        Some(self.original(parser, id).unwrap_or(Cow::Borrowed("")))
    }

    /// Where the row's open unterminated quote is, if it has one: its own
    /// last field, unterminated and unedited, still in the row. Only the
    /// file's last row can have one.
    fn open_quote(&self) -> Option<usize> {
        let Own::File { parsed, .. } = &self.own else {
            return None;
        };
        let fields = parsed.fields();
        let quote = fields.len().checked_sub(1)?;
        let id = CellId::Field(u32::try_from(quote).ok()?);
        let open = fields[quote].unterminated()
            && self.cells.binary_search_by_key(&id, |&(c, _)| c).is_err();
        if !open {
            return None;
        }
        let layout = self.layout();
        (0..layout.len()).find(|&c| layout.get(c) == Some(id))
    }

    /// Takes the hatched cells at the end of an explicit layout that hold
    /// nothing: padding is only ever before an edited hatched cell
    /// (ADR-0005 decision 2, task 2.4b). A default layout's end at its last
    /// edited hatched cell anyway.
    fn trim(&mut self) {
        let Some(layout) = &mut self.layout else {
            return;
        };
        while let Some(&CellId::Appended(j)) = layout.last() {
            let id = CellId::Appended(j);
            if self.cells.binary_search_by_key(&id, |&(c, _)| c).is_ok() {
                break;
            }
            layout.pop();
        }
    }
}

impl Document {
    /// Sets cell (`row`, `column`) to `value`: the user committed an edit.
    /// `row` is the logical row (the header row, if any, is row 0, and can
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
        let (lineage, mut changes) = self.change(None, &[target], false)?;
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
        let (lineage, changes) = self.change(None, &targets, false)?;
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
    /// otherwise nothing changes. A row insert or delete applies by
    /// identity in the edits it was made in, so an undone delete puts back
    /// the same rows, original bytes and all; after a save it applies by
    /// value (ADR-0014 decision 3).
    ///
    /// # Errors
    ///
    /// [`EditError::OtherLineage`] for a command made before the file was
    /// read with another delimiter or encoding, [`EditError::ValueChanged`]
    /// if a cell (or a row) doesn't hold what the command expects, the
    /// errors of [`set_cell`](Self::set_cell), for a row insert or delete
    /// those of [`insert_rows`](Self::insert_rows), and for a column's
    /// those of [`insert_column`](Self::insert_column).
    pub fn apply(&self, command: &Command) -> Result<(), EditError> {
        if command.is_column() {
            return self
                .apply_column(Some(command.lineage), &command.edit)
                .map(|_| ());
        }
        if command.is_structural() {
            return self
                .apply_rows(Some(command.lineage), &command.edit)
                .map(|_| ());
        }
        self.change(Some(command.lineage), &targets(command), false)
            .map(|_| ())
    }

    /// Applies `commands` in order, as [`apply`](Self::apply) does but
    /// whatever their lineage, and says which didn't apply and why: to
    /// recover a failed document's edits, by replaying the app's journal of
    /// commands into a freshly opened document of the same file (ADR-0008
    /// decision 5). A command that doesn't apply is skipped, and the rest
    /// are still tried; one that built on it then usually doesn't apply
    /// either, since its cell doesn't hold the value it expects. Row and
    /// column inserts and deletes apply by value (ADR-0014 decision 3).
    ///
    /// The commands that applied come back in this document's lineage, for
    /// the app's new undo history (a row insert or delete as a new command
    /// of this document's rows).
    ///
    /// Rows the index hasn't reached yet are refused with
    /// [`EditError::NotReadYet`] (and row inserts and deletes with
    /// [`EditError::StillReading`]), so call it once the index job has
    /// finished. It reads each row a command changes: call it off the main
    /// thread for a long history.
    pub fn replay(&self, commands: &[Command]) -> Replay {
        let mut replay = Replay::default();
        for (at, command) in commands.iter().enumerate() {
            let applied = if command.is_column() {
                self.apply_column(None, &command.edit)
            } else if command.is_structural() {
                self.apply_rows(None, &command.edit)
            } else {
                self.change(None, &targets(command), true)
                    .map(|(lineage, changes)| {
                        Some(Command {
                            lineage,
                            edit: replayed(&command.edit, &changes),
                        })
                    })
            };
            match applied {
                Ok(Some(command)) => replay.commands.push(command),
                Ok(None) => {}
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
        if let Some(quote) = work.open_quote()
            && column > quote
        {
            return Err(EditError::AfterUnterminatedQuote { row, column });
        }
        Ok(())
    }

    /// Whether `value` can be saved in the document's encoding, for the app
    /// to say so as it is typed or before it is committed (task 2.3, F5):
    /// `None` if it can, otherwise the first character the encoding can't
    /// represent, as in "“😀” can't be saved in Windows-1252". The edit
    /// itself is allowed either way: Save then refuses, naming the cell,
    /// and Save As UTF-8 writes it (DESIGN §3.7). UTF-16 can represent any
    /// character, so it is always `None` there; such files are read-only
    /// in v1 for another reason.
    #[must_use]
    pub fn unencodable(&self, value: &str) -> Option<crate::save::Unencodable> {
        crate::save::encode(value, self.current().detection.encoding).err()
    }

    /// The lineage of the commands made now (see [`Lineage`]).
    #[must_use]
    pub fn lineage(&self) -> Lineage {
        self.current().edits.lineage()
    }

    /// Whether the document has unsaved edits: some cell reads differently
    /// from the file, or a row is inserted or deleted. Edits set back to
    /// their original values don't count, nor do rows inserted and deleted
    /// again; but a row deleted, and another inserted that reads the same,
    /// does.
    #[must_use]
    pub fn has_edits(&self) -> bool {
        !self.current().edits.is_empty()
    }

    /// The edits' version: it goes up with each edit (each change, undo and
    /// redo, by the rows it touches) and never goes down. A save's
    /// [`snapshot_version`](crate::save::SaveProgress::snapshot_version)
    /// is one: the edits up to it are in the file the save writes, and
    /// those after it aren't. The reading a save makes carries on from the
    /// old reading's version, the edits carried over counted within it (no
    /// version of their own); so does a re-read with another split.
    #[must_use]
    pub fn edit_version(&self) -> u64 {
        u64::try_from(self.current().edits.version()).unwrap_or(u64::MAX)
    }

    /// How many cells are edited (an inserted row's own values don't
    /// count).
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
    /// bytes in the trusted copy differ or aren't there. Each as (logical
    /// row, column), in order; a deleted row's are left out. The edits are
    /// kept and still shown where the row can be read; the app names these
    /// cells (task 2.5) and so does Save As from an incomplete document
    /// (ADR-0008 decision 6). Empty while the first 64 KB can be trusted.
    /// It reads each edited row that came from them.
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
        for (id, edits) in overlay.all() {
            let (Some(hash), Some(physical)) = (edits.head_hash(), id.physical()) else {
                continue;
            };
            let Ok(logical) = overlay.map().logical_of(physical) else {
                continue;
            };
            let row = usize::try_from(physical).unwrap_or(usize::MAX);
            let same = matches!(
                Self::row_bytes(&reading, row),
                Ok(Some(RowBytes { bytes, .. })) if hash_of(&bytes) == hash
            );
            if !same {
                let shown = edits.shown(overlay.columns(), None);
                cells.extend(shown.into_iter().map(|(column, _)| (logical, column)));
            }
        }
        cells.sort_unstable();
        cells
    }

    /// Applies `targets` in order, all or none, and returns the changes
    /// made (cells that already held their value are left out) with the
    /// lineage they were made in. With `lineage`, a command's, it must be
    /// the edits' own. `replaying` a journal, a missing cell a command
    /// means may be an empty field now, as after a save (see [`holds`]).
    fn change(
        &self,
        lineage: Option<Lineage>,
        targets: &[Target<'_>],
        replaying: bool,
    ) -> Result<(Lineage, Vec<CellChange>), EditError> {
        // One change at a time, and none while the file is read again.
        let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let reading = self.current();
        Self::change_in(&reading, lineage, targets, replaying)
    }

    /// [`change`](Self::change) in `reading`, with the writer lock held by
    /// the caller: a save carries edits over to the reading of the file it
    /// wrote before it becomes the current one.
    pub(super) fn change_in(
        reading: &Reading,
        lineage: Option<Lineage>,
        targets: &[Target<'_>],
        replaying: bool,
    ) -> Result<(Lineage, Vec<CellChange>), EditError> {
        let store = &reading.edits;
        if lineage.is_some_and(|lineage| lineage != store.lineage()) {
            return Err(EditError::OtherLineage);
        }
        let overlay = store.overlay();
        let parser = &reading.parser;
        // After a save, a missing cell a command names may be a field now
        // (see `holds`), and so may it in a replay: a row delete undone by
        // value puts a blank line's hatched edit back as an inserted row's
        // own value (task 2.4a).
        let rebased = store.is_rebased() || replaying;
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

        // ADR-0004 decision 8, on each row as it ends up: nothing after an
        // open unterminated quote, in its row or (once rows are inserted
        // or deleted) after its row.
        let rows_now = overlay.map().len();
        for (&row, work) in &rows {
            if let Some(quote) = work.open_quote() {
                let past = quote + 1 < work.layout().len();
                let not_last = rows_now.is_some_and(|len| row + 1 != len);
                if past || not_last {
                    let column = work.last_column;
                    return Err(EditError::AfterUnterminatedQuote { row, column });
                }
            }
        }

        let changed: std::collections::BTreeSet<usize> =
            changes.iter().map(|change| change.row).collect();
        let updates = rows
            .into_iter()
            .filter(|(row, _)| changed.contains(row))
            .map(|(_, work)| (work.id, row_edits(reading, parser, work)))
            .collect();
        // Let go of this snapshot first: the store copies the overlay if
        // anyone else holds it, and this edit mustn't count.
        drop(overlay);
        if !changes.is_empty() {
            store.set_rows(updates);
        }
        Ok((store.lineage(), changes))
    }

    /// Logical row `row`, read for a change.
    fn work<'b>(
        reading: &'b Reading,
        overlay: &Overlay,
        row: usize,
    ) -> Result<Work<'b>, EditError> {
        let slot = overlay.map().slot(row);
        let Some(Slot::Original(physical)) = slot else {
            let Some(Slot::Inserted(n)) = slot else {
                return Err(missing_row(reading, row));
            };
            let id = RowId::inserted(n);
            let inserted = overlay
                .inserted(n)
                .ok_or_else(|| missing_row(reading, row))?;
            let previous = overlay.edits(id).cloned();
            return Ok(Work::of(
                Own::New(Arc::clone(inserted)),
                id,
                false,
                previous,
                overlay,
            ));
        };
        let physical_row = usize::try_from(physical).unwrap_or(usize::MAX);
        // As `Reading::rows_from` decides it.
        let from_head = !reading.head_is_stale() && reading.index.row_count() < reading.head_rows;
        let Some(RowBytes { bytes, base, index }) = Self::row_bytes(reading, physical_row)
            .map_err(|error| EditError::Read { row, error })?
        else {
            return Err(missing_row(reading, row));
        };
        let Some(parsed) = reading
            .parser
            .parse_row_in(index, physical_row, &bytes, base)
        else {
            return Err(missing_row(reading, row));
        };
        let previous = overlay.row_arc(physical_row).cloned();
        let blank = parsed.fields().len() == 1 && parsed.span().is_empty();
        Ok(Work::of(
            Own::File {
                bytes,
                base,
                parsed,
                row: physical_row,
                from_head,
            },
            RowId::original(physical),
            blank,
            previous,
            overlay,
        ))
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

/// A replayed cell edit as this document's command: `edit`, with each
/// missing value it means that a cell holds as an empty field here
/// (`changes`, as made; see [`holds`]) given as that field, so that undoing
/// it here finds what it left.
fn replayed(edit: &Edit, changes: &[CellChange]) -> Edit {
    let made = |change: &CellChange| {
        let mut made = changes
            .iter()
            .filter(|made| (made.row, made.column) == (change.row, change.column));
        (made.next(), made.next_back())
    };
    let fix = |change: &CellChange| {
        let mut change = change.clone();
        let (first, last) = made(&change);
        let last = last.or(first);
        if change.old.is_none() && first.is_some_and(|made| made.old.as_deref() == Some("")) {
            change.old = Some(String::new());
        }
        if change.new.is_none() && last.is_some_and(|made| made.new.as_deref() == Some("")) {
            change.new = Some(String::new());
        }
        change
    };
    match edit {
        Edit::SetCell(change) => Edit::SetCell(fix(change)),
        Edit::SetCells(changes) => Edit::SetCells(changes.iter().map(fix).collect()),
        edit => edit.clone(),
    }
}

/// The column limit: a cell past the row's cells and past
/// [`COLUMN_LIMIT`] can't be edited.
fn check_column(work: &Work<'_>, row: usize, column: usize) -> Result<(), EditError> {
    if column >= work.layout().len().max(work.own_len()).max(COLUMN_LIMIT) {
        return Err(EditError::TooFarRight { row, column });
    }
    Ok(())
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
/// value (`""`, or missing, for a hatched cell; an inserted cell's value)
/// removes the edit; for an inserted row, its own value. Only a hatched
/// cell can be made missing; once the document is `rebased` (see
/// [`holds`]), making one of its fields missing empties it. Padding left
/// at the row's end goes (ADR-0005 decision 2).
fn set(
    work: &mut Work<'_>,
    parser: &RowParser,
    row: usize,
    column: usize,
    value: Option<&str>,
    rebased: bool,
) -> Result<(), EditError> {
    let layout = work.layout();
    let Some(id) = layout.get(column) else {
        // A hatched cell past the row's end: missing, so `""` is no edit.
        let Some(value) = value.filter(|value| !value.is_empty()) else {
            return Ok(());
        };
        let id = match layout.hatched(column) {
            Some(id) => id,
            None => {
                let mut ids = layout.ids().into_owned();
                let mut next = fresh_appended(work.fields, &ids, &work.cells);
                while ids.len() < column {
                    ids.push(CellId::Appended(next));
                    next += 1;
                }
                let id = CellId::Appended(next);
                ids.push(id);
                work.layout = Some(ids);
                id
            }
        };
        let at = work
            .cells
            .binary_search_by_key(&id, |&(c, _)| c)
            .unwrap_or_else(|at| at);
        work.cells.insert(at, (id, Arc::from(value)));
        return Ok(());
    };
    let original = work.original(parser, id).map(Cow::into_owned);
    let value = match value {
        None if rebased && original.is_some() => Some(""),
        value => value,
    };
    let back = match (original.as_deref(), value) {
        (Some(original), Some(value)) => original == value,
        (Some(_), None) => return Err(EditError::ValueChanged { row, column }),
        (None, Some(value)) => value.is_empty(),
        (None, None) => true,
    };
    match (work.cells.binary_search_by_key(&id, |&(c, _)| c), back) {
        (Ok(at), true) => {
            work.cells.remove(at);
        }
        (Ok(at), false) => work.cells[at].1 = Arc::from(value.unwrap_or_default()),
        (Err(_), true) => {}
        (Err(at), false) => work
            .cells
            .insert(at, (id, Arc::from(value.unwrap_or_default()))),
    }
    work.trim();
    Ok(())
}

/// A changed row's new edits, or `None` if it has none left.
fn row_edits(reading: &Reading, parser: &RowParser, mut work: Work<'_>) -> Option<RowEdits> {
    // A written-out layout stays while column operations are in effect,
    // even if it reads as the default one now: an undo that takes out an
    // operation the row went its own way for restores it (`edit::columns`).
    if work.cells.is_empty() && work.layout.is_none() {
        return None;
    }
    let fields = work.fields;
    let blank = work.blank;
    let layout = work.layout.take().map(Arc::from);
    let Own::File {
        bytes,
        base,
        parsed,
        row,
        from_head,
    } = &work.own
    else {
        // An inserted row's values are text: no diagnostics of their own
        // but a NUL, which the edits' marks check on every value anyway.
        return Some(RowEdits::new(fields, work.cells, Arc::from([]), None).with_layout(layout));
    };
    // The row's own fields' diagnostics, worked out the first time it is
    // edited: none to look for if its marks say it has none.
    let flagged = match &work.previous {
        Some(previous) => Arc::clone(previous.flagged()),
        None => {
            let unflagged = reading.diagnostics.get().is_some_and(|diagnostics| {
                *row < diagnostics.marked_rows_and_mode().0
                    && !diagnostics.row_is(*row, Mark::Flagged)
            });
            if unflagged {
                Arc::from([])
            } else {
                let id = RowId::original(u32::try_from(*row).unwrap_or(u32::MAX));
                let view = RowView::own(parser, bytes, *base, parsed, id);
                Arc::from(view.flagged_fields())
            }
        }
    };
    let head_hash = match &work.previous {
        Some(previous) => previous.head_hash(),
        None => from_head.then(|| hash_of(bytes)),
    };
    Some(
        RowEdits::new(fields, work.cells, flagged, head_hash)
            .blank(blank)
            .with_layout(layout),
    )
}

/// Why logical row `row` can't be edited: it isn't there (the index is
/// complete without it); it can't be read, because the file changed while
/// it was read or was deleted on its share, so the row will never come; or
/// it hasn't been read yet (the index hasn't reached it, or stopped before
/// it: its drive may come back).
pub(super) fn missing_row(reading: &Reading, row: usize) -> EditError {
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
