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
//! Rows are physical rows (the header row, if any, is row 0, and can be
//! edited), as everywhere in this crate. A missing cell (past the end of
//! its row: a hatched cell) is `nil`, not `""`.

use leal_core::edit::{self, CellChange, Command, Edit, EditError, Lineage};

use super::{Document, read_error, to_index, to_u32, to_u64, to_usize};
use crate::LealError;

#[cfg(test)]
mod tests;

/// One cell's change, in a command: where, and its value before and after.
/// `nil` is a missing cell.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ValueChange {
    /// The physical row.
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
    /// batch (all or nothing).
    pub changes: Vec<ValueChange>,
}

/// One cell to set, for [`Document::set_cells`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CellEdit {
    /// The physical row.
    pub row: u64,
    /// The column.
    pub column: u32,
    /// The new value.
    pub value: String,
}

/// A cell, for [`Document::edit_conflicts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CellPlace {
    /// The physical row.
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
        let changes = match command.edit {
            Edit::SetCell(change) => vec![change],
            Edit::SetCells(changes) => changes,
        };
        EditCommand {
            lineage: command.lineage.get(),
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

    /// Undoes `command`: each cell goes back to its old value, if it still
    /// holds the new one, the last first.
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

    /// The edits' version: how many edits (each change, undo and redo) the
    /// current reading's edits have had. A save's
    /// `SaveProgress.snapshot_version` is one; the app notes it after each
    /// edit, with NSDocument's `changeCountToken`, to know which token the
    /// saved file matches (task 2.5).
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
