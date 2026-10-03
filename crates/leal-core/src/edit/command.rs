//! The commands that change a document, and why one may not apply.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Column, Rows};
use crate::source::ReadError;

/// The furthest a hatched-cell edit may reach: column `COLUMN_LIMIT - 1`,
/// unless the row itself is longer. It guards against a row being padded
/// to millions of empty cells by mistake; Excel's own limit is 16,384
/// columns.
pub const COLUMN_LIMIT: usize = 1 << 20;

/// Which set of edits a command belongs to: a document's edits, while the
/// file is split into cells one way (ADR-0008 decision 4).
///
/// A command applies only to the edits it was made in. Reading the file
/// with another delimiter or encoding starts a new lineage, so an undo or
/// redo kept from before can't land on a cell that merely holds the same
/// value under the new split. The header toggle and a drive reconnecting
/// keep the lineage, and so will task 2.2's rebase onto a saved file.
/// [`Document::replay`](crate::document::Document::replay) checks values
/// only, and gives back the commands in the document's own lineage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lineage(u64);

impl Lineage {
    /// A lineage no other one has been or will be.
    pub(crate) fn new() -> Lineage {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Lineage(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The lineage as a number, for the FFI.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    /// The lineage a number from [`get`](Self::get) stands for.
    #[must_use]
    pub fn from_raw(raw: u64) -> Lineage {
        Lineage(raw)
    }
}

/// One cell's change: where, and its value before and after. `None` is a
/// missing cell: past the end of its row (a hatched cell, ADR-0005
/// decision 2), which is not the same as an empty one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CellChange {
    /// The row, logical (in 2.1, the physical row: the header row, if any,
    /// is row 0).
    pub row: usize,
    /// The column: a field of the row, or a cell past its end.
    pub column: usize,
    /// The value before.
    pub old: Option<String>,
    /// The value after: what the cell reads as once the change is made.
    /// Setting a hatched cell back to `""` leaves it missing, so this is
    /// `None` then.
    pub new: Option<String>,
}

impl CellChange {
    /// The change that undoes this one.
    #[must_use]
    pub fn inverse(&self) -> CellChange {
        self.clone().into_inverse()
    }

    /// [`inverse`](Self::inverse), reusing this change's values.
    #[must_use]
    pub fn into_inverse(self) -> CellChange {
        CellChange {
            row: self.row,
            column: self.column,
            old: self.new,
            new: self.old,
        }
    }
}

/// What a command changes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Edit {
    /// One cell.
    SetCell(CellChange),
    /// Several cells at once, in order, as one undo step: a paste, or
    /// clearing a selection (task 2.6). All of them apply, or none.
    SetCells(Vec<CellChange>),
    /// Rows inserted at logical row `at` (task 2.4a): new rows, or, as the
    /// inverse of a delete, the rows it took out, back with their edits.
    InsertRows {
        /// The first row's logical row.
        at: usize,
        /// The rows.
        rows: Arc<Rows>,
    },
    /// Logical rows `at..at + rows.len()` deleted (task 2.4a), keeping
    /// them (and their edits) for undo.
    DeleteRows {
        /// The first row deleted.
        at: usize,
        /// The rows.
        rows: Arc<Rows>,
    },
    /// A column inserted before logical column `at` (task 2.4b): a cell
    /// in every row long enough (oracle rule 6: at least `at` cells, not a
    /// blank line), or, as the inverse of a column delete, the cells it
    /// took put back.
    InsertColumn {
        /// The column.
        at: usize,
        /// The operation and the rows it applied to.
        column: Arc<Column>,
    },
    /// Logical column `at` deleted from every row that has it (task 2.4b),
    /// or, as the inverse of a column insert, the cells it gave taken out.
    DeleteColumn {
        /// The column.
        at: usize,
        /// The operation and the rows it applied to.
        column: Arc<Column>,
    },
}

/// A change to a document, with what it replaced, so it can be undone,
/// redone and replayed (DESIGN §3.6): cell edits, row inserts and deletes
/// (task 2.4a) and column inserts and deletes (task 2.4b).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Command {
    /// The edits it belongs to.
    pub lineage: Lineage,
    /// What it changes.
    pub edit: Edit,
}

impl Command {
    /// The command that undoes this one: each cell from its new value back
    /// to its old, the last first; a row insert's is the delete of the same
    /// rows, and the other way round.
    #[must_use]
    pub fn inverse(&self) -> Command {
        self.clone().into_inverse()
    }

    /// [`inverse`](Self::inverse), reusing this command's values.
    #[must_use]
    pub fn into_inverse(self) -> Command {
        let edit = match self.edit {
            Edit::SetCell(change) => Edit::SetCell(change.into_inverse()),
            Edit::SetCells(changes) => Edit::SetCells(
                changes
                    .into_iter()
                    .rev()
                    .map(CellChange::into_inverse)
                    .collect(),
            ),
            Edit::InsertRows { at, rows } => Edit::DeleteRows { at, rows },
            Edit::DeleteRows { at, rows } => Edit::InsertRows { at, rows },
            Edit::InsertColumn { at, column } => Edit::DeleteColumn { at, column },
            Edit::DeleteColumn { at, column } => Edit::InsertColumn { at, column },
        };
        Command {
            lineage: self.lineage,
            edit,
        }
    }

    /// The cells it changes, in order: none for a row insert or delete.
    #[must_use]
    pub fn changes(&self) -> &[CellChange] {
        match &self.edit {
            Edit::SetCell(change) => std::slice::from_ref(change),
            Edit::SetCells(changes) => changes,
            Edit::InsertRows { .. }
            | Edit::DeleteRows { .. }
            | Edit::InsertColumn { .. }
            | Edit::DeleteColumn { .. } => &[],
        }
    }

    /// Whether it inserts or deletes rows or a column.
    #[must_use]
    pub fn is_structural(&self) -> bool {
        !matches!(self.edit, Edit::SetCell(_) | Edit::SetCells(_))
    }

    /// Whether it inserts or deletes a column.
    #[must_use]
    pub fn is_column(&self) -> bool {
        matches!(
            self.edit,
            Edit::InsertColumn { .. } | Edit::DeleteColumn { .. }
        )
    }
}

/// Why an edit, or a command, wasn't applied. The document is then
/// unchanged: a command of several cells applies all of them or none.
/// Each names the first cell that couldn't be changed.
#[derive(Debug)]
pub enum EditError {
    /// The file has no such row.
    NoSuchRow {
        /// The row.
        row: usize,
    },
    /// The row hasn't been read: the index hasn't reached it yet, or
    /// stopped before it (a drive disconnected). Ask again once it has.
    NotReadYet {
        /// The row.
        row: usize,
    },
    /// The cell is past [`COLUMN_LIMIT`] and past the end of its row.
    TooFarRight {
        /// The row.
        row: usize,
        /// The column.
        column: usize,
    },
    /// The edit would put bytes after an unterminated quote, inside it
    /// (ADR-0004 decision 8): a cell past it, or setting it back to its
    /// original value while a cell past it is edited.
    AfterUnterminatedQuote {
        /// The row.
        row: usize,
        /// The column.
        column: usize,
    },
    /// The cell doesn't hold the command's old value, so the command no
    /// longer applies (ADR-0008 decision 5): the file changed, or the
    /// command was made for different edits.
    ValueChanged {
        /// The row.
        row: usize,
        /// The column.
        column: usize,
    },
    /// The command belongs to another [`Lineage`]: it was made before the
    /// file was read with another delimiter or encoding.
    OtherLineage,
    /// Rows and columns can't be inserted or deleted until the whole file
    /// has been read (and, on a removable drive or a share, copied) and
    /// can be trusted (ADR-0014 decision 1). Ask again once the index is
    /// complete.
    StillReading,
    /// Rows and columns can't be inserted or deleted while a save runs
    /// (ADR-0014 decision 1); that includes undoing or redoing such a
    /// command. Ask again once it has finished.
    Saving,
    /// No row has that column: a column is inserted at most just past the
    /// widest row, and deleted only where some row has it (task 2.4b).
    NoSuchColumn {
        /// The column.
        column: usize,
    },
    /// The row couldn't be read (see `Document::rows`).
    Read {
        /// The row.
        row: usize,
        /// Why.
        error: ReadError,
    },
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoSuchRow { row } => write!(f, "the file has no row {row}"),
            EditError::NotReadYet { row } => write!(f, "row {row} hasn't been read"),
            EditError::TooFarRight { row, column } => write!(
                f,
                "row {row}, column {column} is past the end of its row and past column {COLUMN_LIMIT}"
            ),
            EditError::AfterUnterminatedQuote { row, column } => write!(
                f,
                "editing row {row}, column {column} would put text inside an unterminated quote"
            ),
            EditError::ValueChanged { row, column } => write!(
                f,
                "row {row}, column {column} no longer holds the value the edit replaced"
            ),
            EditError::OtherLineage => {
                f.write_str("the edit was made before the file was read another way")
            }
            EditError::StillReading => f.write_str(
                "rows and columns can't be inserted or deleted until the whole file is read",
            ),
            EditError::Saving => {
                f.write_str("rows and columns can't be inserted or deleted while saving")
            }
            EditError::NoSuchColumn { column } => write!(f, "no row has column {column}"),
            EditError::Read { row, error } => write!(f, "row {row} couldn't be read: {error}"),
        }
    }
}

impl std::error::Error for EditError {}

/// What [`Document::replay`](crate::document::Document::replay) did.
#[derive(Debug, Default)]
pub struct Replay {
    /// The commands that applied, in order, in the document's own lineage:
    /// the app's new undo history.
    pub commands: Vec<Command>,
    /// The commands that didn't, by their position in the list passed, and
    /// why: for the app to name them (ADR-0008 decision 5).
    pub refused: Vec<(usize, EditError)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(row: usize, old: Option<&str>, new: Option<&str>) -> CellChange {
        CellChange {
            row,
            column: 1,
            old: old.map(str::to_owned),
            new: new.map(str::to_owned),
        }
    }

    #[test]
    fn an_inverse_swaps_the_values() {
        let command = Command {
            lineage: Lineage::new(),
            edit: Edit::SetCell(change(3, Some("a"), None)),
        };
        assert_eq!(
            command.inverse().changes(),
            [change(3, None, Some("a"))],
            "a hatched cell goes back to missing"
        );
        assert_eq!(command.inverse().lineage, command.lineage);
        assert_eq!(command.inverse().inverse(), command);
    }

    #[test]
    fn a_batch_is_undone_last_first() {
        let command = Command {
            lineage: Lineage::new(),
            edit: Edit::SetCells(vec![
                change(1, Some("a"), Some("b")),
                change(1, Some("b"), Some("c")),
            ]),
        };
        assert_eq!(
            command.into_inverse().changes(),
            [
                change(1, Some("c"), Some("b")),
                change(1, Some("b"), Some("a"))
            ]
        );
    }

    #[test]
    fn every_lineage_is_new() {
        let (a, b) = (Lineage::new(), Lineage::new());
        assert_ne!(a, b);
        assert_eq!(Lineage::from_raw(a.get()), a);
    }
}
