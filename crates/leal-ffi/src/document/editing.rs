//! Editing, for Swift (task 2.1). See `leal_core::edit` and
//! `leal_core::document::Document::set_cell`.
//!
//! The core keeps no undo stack: the app's `NSUndoManager` does (task
//! 2.5). [`Document::set_cell`] and [`Document::set_cells`] return the
//! [`EditCommand`] they made, which the app registers for undo; ⌘Z calls
//! [`Document::undo`] with it, and ⇧⌘Z [`Document::redo`].
//!
//! **Recovery needs a journal of the app's own.** An `NSUndoManager` can't
//! be listed, so the app also appends each command it applies (an edit, an
//! undo, a redo) to a journal, together with the reading's choices
//! (delimiter, encoding, header). If the document fails (DESIGN §3.9),
//! **Recover changes** opens the file afresh with those choices, waits for
//! its index, and passes the journal, oldest first, to
//! [`Document::replay`] (ADR-0008 decision 5). The commands that applied
//! come back in the new document's lineage, for a new undo history.
//!
//! A command applies only in the edits it was made in (its `lineage`):
//! after **Treat As** or **Reopen with Encoding**, older commands are
//! refused with [`EditRefusal::OtherLineage`], so the app clears its undo
//! stack then.
//!
//! **Rows** (task 2.4a). [`Document::insert_rows`] and
//! [`Document::delete_rows`] give commands too, undone and redone the same
//! way: their [`EditCommand::structural`] holds the rows, which the app
//! keeps but can't look into beyond [`StructuralEdit`]'s summary. They need the whole
//! file read, and no save running ([`Document::can_change_rows`]).
//!
//! **Columns** (task 2.4b). [`Document::insert_column`] and
//! [`Document::delete_column`] likewise, their [`StructuralEdit`] holding
//! the column; they need what row inserts need, and a search starts again
//! after one ([`Document::can_insert_column`],
//! [`Document::can_delete_column`]).
//!
//! Rows are logical rows: as the document has them now, after any rows
//! inserted or deleted (the header row, if any, is row 0, and can be
//! edited). A missing cell (past the end of its row: a hatched cell) is
//! `nil`, not `""`.

use std::sync::Arc;

use leal_core::edit::{self, CellChange, Command, Edit, EditError, Lineage};

use super::{Document, TextEncoding, read_error, to_index, to_u32, to_u64, to_usize};
use crate::LealError;

#[cfg(test)]
mod tests;

/// A character the document's encoding can't represent, so a Save would
/// refuse the value (F5). See `leal_core::save::Unencodable`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UnencodableCharacter {
    /// The character (one Unicode scalar; it may be several UTF-16 units
    /// in Swift).
    pub character: String,
    /// The document's encoding.
    pub encoding: TextEncoding,
}

/// One cell's change, in a command: where, and its value before and after.
/// `nil` is a missing cell.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ValueChange {
    /// The logical row.
    pub row: u64,
    /// The column: a field of the row, or a cell past its end.
    pub column: u32,
    /// The value before; `nil` if the cell was missing.
    pub old_value: Option<String>,
    /// The value after; `nil` if the cell is missing now.
    pub new_value: Option<String>,
}

/// A change to a document, with what it replaced: what the app's undo
/// history and its journal hold. See `leal_core::edit::Command`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EditCommand {
    /// The edits it belongs to ([`Document::lineage`]).
    pub lineage: u64,
    /// The cells it changes, in order: one for an edit, several for a
    /// batch (all or nothing). Empty for a row insert or delete.
    pub changes: Vec<ValueChange>,
    /// The rows it inserts or deletes, for a row insert or delete, or the
    /// column, for a column insert or delete.
    pub structural: Option<Arc<StructuralEdit>>,
}

/// A row insert or delete (task 2.4a), or a column's (task 2.4b), inside an
/// [`EditCommand`]: opaque to the app, which keeps it for undo and redo,
/// but for where the rows or the column are. See
/// `leal_core::edit::Edit::InsertRows` and `Edit::InsertColumn`.
#[derive(Debug, PartialEq, Eq, uniffi::Object)]
pub struct StructuralEdit {
    edit: Edit,
}

#[uniffi::export]
impl StructuralEdit {
    /// Whether it inserts rows or a column (its undo deletes them),
    /// rather than deleting them.
    #[must_use]
    pub fn inserts(&self) -> bool {
        matches!(
            self.edit,
            Edit::InsertRows { .. } | Edit::InsertColumn { .. }
        )
    }

    /// Whether it inserts or deletes a column, rather than rows.
    #[must_use]
    pub fn is_column(&self) -> bool {
        matches!(
            self.edit,
            Edit::InsertColumn { .. } | Edit::DeleteColumn { .. }
        )
    }

    /// The first row's logical row: 0 for a column.
    #[must_use]
    pub fn first_row(&self) -> u64 {
        match &self.edit {
            Edit::InsertRows { at, .. } | Edit::DeleteRows { at, .. } => to_u64(*at),
            _ => 0,
        }
    }

    /// How many rows: those inserted or deleted, or those a column insert
    /// or delete applied to.
    #[must_use]
    pub fn row_count(&self) -> u64 {
        match &self.edit {
            Edit::InsertRows { rows, .. } | Edit::DeleteRows { rows, .. } => to_u64(rows.len()),
            Edit::InsertColumn { column, .. } | Edit::DeleteColumn { column, .. } => {
                to_u64(column.rows())
            }
            Edit::SetCell(_) | Edit::SetCells(_) => 0,
        }
    }

    /// The logical column, for a column insert or delete.
    #[must_use]
    pub fn column(&self) -> Option<u32> {
        match &self.edit {
            Edit::InsertColumn { at, .. } | Edit::DeleteColumn { at, .. } => Some(to_u32(*at)),
            _ => None,
        }
    }
}

/// One cell to set, for [`Document::set_cells`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CellEdit {
    /// The logical row.
    pub row: u64,
    /// The column.
    pub column: u32,
    /// The new value.
    pub value: String,
}

/// A cell, for [`Document::edit_conflicts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CellPlace {
    /// The logical row.
    pub row: u64,
    /// The column.
    pub column: u32,
}

/// Why an edit, undo, redo or replayed command wasn't applied. See
/// `leal_core::edit::EditError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EditRefusal {
    /// The file has no such row.
    NoSuchRow,
    /// The row hasn't been read: the index hasn't reached it, or stopped
    /// before it (a drive disconnected). Try again once it has.
    NotReadYet,
    /// The cell is too far past the end of its row.
    TooFarRight,
    /// It would put text inside an unterminated quote (ADR-0004 decision
    /// 8): a cell after the quote's, while the quote is open.
    AfterUnterminatedQuote,
    /// A cell doesn't hold the value the command replaced, so the command
    /// no longer applies (the file changed).
    ValueChanged,
    /// The command was made before the file was read with another
    /// delimiter or encoding.
    OtherLineage,
    /// Rows and columns can't be inserted or deleted until the whole file
    /// has been read (ADR-0014 decision 1): disable Insert and Delete Row
    /// and Column, saying so, and try again once the index is complete.
    StillReading,
    /// Rows and columns can't be inserted or deleted, nor such a command
    /// undone or redone, while a save runs (ADR-0014 decision 1).
    Saving,
    /// No row has the column: a column is inserted at most just past the
    /// widest row, and deleted only where a row has it.
    NoSuchColumn,
    /// The row couldn't be read (its drive or share went away).
    Unreadable,
}

/// A command [`Document::replay`] didn't apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct RefusedCommand {
    /// Its position in the list passed.
    pub index: u32,
    /// Why.
    pub refusal: EditRefusal,
}

/// What [`Document::replay`] did.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReplayReport {
    /// The commands that applied, in order, in this document's lineage:
    /// the new undo history.
    pub applied: Vec<EditCommand>,
    /// The ones that didn't, in order, for the app to name.
    pub refused: Vec<RefusedCommand>,
}

impl From<Command> for EditCommand {
    fn from(command: Command) -> Self {
        let (changes, structural) = match command.edit {
            Edit::SetCell(change) => (vec![change], None),
            Edit::SetCells(changes) => (changes, None),
            edit @ (Edit::InsertRows { .. }
            | Edit::DeleteRows { .. }
            | Edit::InsertColumn { .. }
            | Edit::DeleteColumn { .. }) => (Vec::new(), Some(Arc::new(StructuralEdit { edit }))),
        };
        EditCommand {
            lineage: command.lineage.get(),
            structural,
            changes: changes
                .into_iter()
                .map(|change| ValueChange {
                    row: to_u64(change.row),
                    column: to_u32(change.column),
                    old_value: change.old,
                    new_value: change.new,
                })
                .collect(),
        }
    }
}

impl From<EditCommand> for Command {
    fn from(command: EditCommand) -> Self {
        if let Some(structural) = command.structural {
            return Command {
                lineage: Lineage::from_raw(command.lineage),
                edit: structural.edit.clone(),
            };
        }
        let mut changes: Vec<CellChange> = command
            .changes
            .into_iter()
            .map(|change| CellChange {
                row: to_index(change.row),
                column: to_usize(change.column),
                old: change.old_value,
                new: change.new_value,
            })
            .collect();
        // One cell or several: the same to undo, redo and replay.
        let edit = match changes.len() {
            1 => Edit::SetCell(changes.remove(0)),
            _ => Edit::SetCells(changes),
        };
        Command {
            lineage: Lineage::from_raw(command.lineage),
            edit,
        }
    }
}

/// The refusal, and the cell it names.
fn refusal(error: &EditError) -> (EditRefusal, Option<usize>, Option<usize>) {
    match *error {
        EditError::NoSuchRow { row } => (EditRefusal::NoSuchRow, Some(row), None),
        EditError::NotReadYet { row } => (EditRefusal::NotReadYet, Some(row), None),
        EditError::TooFarRight { row, column } => {
            (EditRefusal::TooFarRight, Some(row), Some(column))
        }
        EditError::AfterUnterminatedQuote { row, column } => {
            (EditRefusal::AfterUnterminatedQuote, Some(row), Some(column))
        }
        EditError::ValueChanged { row, column } => {
            (EditRefusal::ValueChanged, Some(row), Some(column))
        }
        EditError::OtherLineage => (EditRefusal::OtherLineage, None, None),
        EditError::StillReading => (EditRefusal::StillReading, None, None),
        EditError::Saving => (EditRefusal::Saving, None, None),
        EditError::NoSuchColumn { column } => (EditRefusal::NoSuchColumn, None, Some(column)),
        EditError::Read { row, .. } => (EditRefusal::Unreadable, Some(row), None),
    }
}

impl Document {
    /// A refusal for Swift: the usual read error for a row that couldn't
    /// be read, otherwise [`LealError::EditRefused`].
    fn edit_error(&self, error: &EditError) -> LealError {
        if let EditError::Read { error, .. } = error {
            return read_error(&self.path, error);
        }
        let (refusal, row, column) = refusal(error);
        LealError::EditRefused {
            path: self.path.clone(),
            refusal,
            row: row.map(to_u64),
            column: column.map(to_u32),
        }
    }
}

#[uniffi::export]
impl Document {
    /// Sets the cell at physical row `row` and column `column` to `value`:
    /// the user committed an edit. The column may be past the end of a
    /// short or blank row (a hatched cell, ADR-0005 decision 2). Returns
    /// the command, to register for undo, or `nil` if the cell already
    /// reads as `value` (`""` in a missing cell included): committing an
    /// unchanged value is no edit (ADR-0008 decision 3). Fast enough for
    /// the main thread.
    ///
    /// # Errors
    ///
    /// [`LealError::EditRefused`], with the document unchanged; the read
    /// errors of [`rows`](Document::rows); [`LealError::DocumentFailed`].
    pub fn set_cell(
        &self,
        row: u64,
        column: u32,
        value: &str,
    ) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            self.document
                .set_cell(to_index(row), to_usize(column), value)
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Sets several cells at once, in order, as one command (a paste, or
    /// clearing a selection): all of them, or none. `nil` if no cell
    /// changes.
    ///
    /// # Errors
    ///
    /// As for [`set_cell`](Self::set_cell), naming the first cell that
    /// couldn't be changed; nothing changes then.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "UniFFI passes a list from Swift by value"
    )]
    pub fn set_cells(&self, cells: Vec<CellEdit>) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            let cells: Vec<(usize, usize, &str)> = cells
                .iter()
                .map(|cell| {
                    (
                        to_index(cell.row),
                        to_usize(cell.column),
                        cell.value.as_str(),
                    )
                })
                .collect();
            self.document
                .set_cells(&cells)
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Inserts `rows` (each a row's values; an empty one has one empty
    /// cell) before logical row `at` (the row count appends them), as one
    /// command for the undo manager. A row inserted at 0 becomes the header
    /// row when the file has one, so the app's **Insert Row Above** on the
    /// first data row inserts at 1. `nil` if `rows` is empty. Fast enough
    /// for the main thread.
    ///
    /// # Errors
    ///
    /// [`LealError::EditRefused`] with [`EditRefusal::StillReading`],
    /// [`EditRefusal::Saving`], [`EditRefusal::NoSuchRow`] or
    /// [`EditRefusal::AfterUnterminatedQuote`], with the document
    /// unchanged; [`LealError::DocumentFailed`].
    #[expect(
        clippy::needless_pass_by_value,
        reason = "UniFFI passes a list from Swift by value"
    )]
    pub fn insert_rows(
        &self,
        at: u64,
        rows: Vec<Vec<String>>,
    ) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            self.document
                .insert_rows(to_index(at), &rows)
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Deletes logical rows `at..at + count`, as one command for the undo
    /// manager: undo puts the same rows back, their original bytes and
    /// edits included. `nil` if `count` is 0.
    ///
    /// # Errors
    ///
    /// As for [`insert_rows`](Self::insert_rows).
    pub fn delete_rows(&self, at: u64, count: u64) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            self.document
                .delete_rows(to_index(at), to_index(count))
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Whether rows can be inserted or deleted now, for enabling **Insert
    /// Row** and **Delete Row**: `nil` if they can, otherwise why not
    /// ([`EditRefusal::StillReading`] or [`EditRefusal::Saving`]).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_change_rows(&self) -> Result<Option<EditRefusal>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .can_change_rows()
                .err()
                .map(|error| refusal(&error).0))
        })
    }

    /// Whether rows can be inserted before logical row `at` now: `nil` if
    /// they can, otherwise why not (as [`can_change_rows`], or after an
    /// unterminated quote's row).
    ///
    /// [`can_change_rows`]: Self::can_change_rows
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_insert_rows(&self, at: u64) -> Result<Option<EditRefusal>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .can_insert_rows(to_index(at))
                .err()
                .map(|error| refusal(&error).0))
        })
    }

    /// Inserts a column before logical column `at`, each new cell holding
    /// `value`, in every row with at least `at` cells (a shorter row or a
    /// blank line is left as it is), as one command for the undo manager.
    /// `at` may be the widest row's length. A search starts again. It
    /// reads no row (about a millisecond per million rows).
    ///
    /// # Errors
    ///
    /// [`LealError::EditRefused`] with [`EditRefusal::StillReading`],
    /// [`EditRefusal::Saving`], [`EditRefusal::NoSuchColumn`] or
    /// [`EditRefusal::AfterUnterminatedQuote`], with the document
    /// unchanged; [`LealError::DocumentFailed`].
    #[expect(
        clippy::needless_pass_by_value,
        reason = "UniFFI passes a string from Swift by value"
    )]
    pub fn insert_column(&self, at: u32, value: String) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            self.document
                .insert_column(to_usize(at), &value)
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Deletes logical column `at` from every row that has it, as one
    /// command for the undo manager: undo puts the same cells back, their
    /// original bytes and edits included.
    ///
    /// # Errors
    ///
    /// As for [`insert_column`](Self::insert_column).
    pub fn delete_column(&self, at: u32) -> Result<Option<EditCommand>, LealError> {
        self.call(|| {
            self.document
                .delete_column(to_usize(at))
                .map(|command| command.map(EditCommand::from))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Whether a column can be inserted before logical column `at` now,
    /// for enabling **Insert Column**: `nil` if it can, otherwise why not.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_insert_column(&self, at: u32) -> Result<Option<EditRefusal>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .can_insert_column(to_usize(at))
                .err()
                .map(|error| refusal(&error).0))
        })
    }

    /// Whether logical column `at` can be deleted now, for enabling
    /// **Delete Column**: `nil` if it can, otherwise why not.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_delete_column(&self, at: u32) -> Result<Option<EditRefusal>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .can_delete_column(to_usize(at))
                .err()
                .map(|error| refusal(&error).0))
        })
    }

    /// Whether the cell can be edited now, for deciding before the in-cell
    /// editor opens: `nil` if it can, otherwise why not.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_edit(&self, row: u64, column: u32) -> Result<Option<EditRefusal>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .can_edit(to_index(row), to_usize(column))
                .err()
                .map(|error| refusal(&error).0))
        })
    }

    /// Whether `value` can be saved in the document's encoding (task 2.3,
    /// F5), for saying so as it is typed or before it is committed: `nil` if
    /// it can, otherwise the first character the encoding can't represent,
    /// as in "“😀” can't be saved in Windows-1252". The edit is allowed
    /// either way: Save then refuses it ([`SaveFailure::Unencodable`]),
    /// and Save As UTF-8 writes it (DESIGN §3.7).
    ///
    /// [`SaveFailure::Unencodable`]: crate::document::saving::SaveFailure::Unencodable
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn unencodable(&self, value: &str) -> Result<Option<UnencodableCharacter>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .unencodable(value)
                .map(|refused| UnencodableCharacter {
                    character: refused.character.to_string(),
                    encoding: refused.encoding.into(),
                }))
        })
    }

    /// Undoes `command`: each cell goes back to its old value, if it still
    /// holds the new one, the last first; inserted rows go, deleted rows
    /// come back.
    ///
    /// # Errors
    ///
    /// [`LealError::EditRefused`] with [`EditRefusal::ValueChanged`] or
    /// [`EditRefusal::OtherLineage`] if it doesn't apply, and as for
    /// [`set_cell`](Self::set_cell).
    pub fn undo(&self, command: EditCommand) -> Result<(), LealError> {
        self.call(|| {
            self.document
                .apply(&Command::from(command).into_inverse())
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Redoes `command`: each cell gets its new value again, if it still
    /// holds the old one.
    ///
    /// # Errors
    ///
    /// As for [`undo`](Self::undo).
    pub fn redo(&self, command: EditCommand) -> Result<(), LealError> {
        self.call(|| {
            self.document
                .apply(&Command::from(command))
                .map_err(|error| self.edit_error(&error))
        })
    }

    /// Applies the app's journal of `commands`, oldest first, into this
    /// document, freshly opened, to recover a failed document's edits
    /// (ADR-0008 decision 5), whatever their lineage, and says which didn't
    /// apply. Call it once the index job has finished, off the main thread
    /// for a long journal.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`]. A command that can't be applied is
    /// reported, not an error.
    pub fn replay(&self, commands: Vec<EditCommand>) -> Result<ReplayReport, LealError> {
        self.call(|| {
            let commands: Vec<Command> = commands.into_iter().map(Command::from).collect();
            let replay: edit::Replay = self.document.replay(&commands);
            Ok(ReplayReport {
                applied: replay.commands.into_iter().map(EditCommand::from).collect(),
                refused: replay
                    .refused
                    .iter()
                    .map(|(index, error)| RefusedCommand {
                        index: to_u32(*index),
                        refusal: refusal(error).0,
                    })
                    .collect(),
            })
        })
    }

    /// The lineage of the commands made now: it changes when the file is
    /// read with another delimiter or encoding.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn lineage(&self) -> Result<u64, LealError> {
        self.call(|| Ok(self.document.lineage().get()))
    }

    /// Whether the document has unsaved edits: some cell reads differently
    /// from the file. For the dirty state, and for disabling **Treat As**
    /// and **Reopen with Encoding** (ADR-0008 decision 4).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn has_unsaved_edits(&self) -> Result<bool, LealError> {
        self.call(|| Ok(self.document.has_edits()))
    }

    /// The edits' version: it goes up with each edit (each change, undo and
    /// redo, by the rows it touches) and never goes down, across saves and
    /// re-reads too (a save's carry-over has no version of its own). A
    /// save's `SaveProgress.snapshot_version` is one; the app notes it after
    /// each edit, with NSDocument's `changeCountToken`, to know which token
    /// the saved file matches (task 2.5).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn edit_version(&self) -> Result<u64, LealError> {
        self.call(|| Ok(self.document.edit_version()))
    }

    /// How many cells are edited.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn edited_cell_count(&self) -> Result<u64, LealError> {
        self.call(|| Ok(to_u64(self.document.edited_cells())))
    }

    /// The edited cells whose rows can't be trusted to be what they were
    /// edited from: rows edited from the first 64 KB before the file turned
    /// out to change while it was read, which the trusted copy doesn't have
    /// (task 1.9). The edits are kept; the app names these cells, and so
    /// does Save As from an incomplete document (ADR-0008 decision 6).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn edit_conflicts(&self) -> Result<Vec<CellPlace>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .edit_conflicts()
                .into_iter()
                .map(|(row, column)| CellPlace {
                    row: to_u64(row),
                    column: to_u32(column),
                })
                .collect())
        })
    }

    /// The cell's whole display value as it reads now, for the in-cell
    /// editor and the inspector to start from (ADR-0008 decision 3): never
    /// the grid's shortened text or its symbols. Empty for a cell past the
    /// end of its row; `nil` if the row can't be read yet. Call it off the
    /// main thread if the value may be long.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Document::rows).
    pub fn full_value(&self, row: u64, column: u32) -> Result<Option<String>, LealError> {
        self.call(|| {
            self.document
                .full_value(to_index(row), to_usize(column))
                .map_err(|error| read_error(&self.path, &error))
        })
    }
}
