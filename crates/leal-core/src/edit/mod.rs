//! Edits (DESIGN §3.6, task 2.1): the commands that change a document, and
//! the overlay that holds what they changed. The original bytes are never
//! modified.
//!
//! **Commands** (`command.rs`, public). A [`Command`] says what changed, in
//! logical coordinates, with each cell's value before and after:
//! [`Edit::SetCell`] sets one cell, and [`Edit::SetCells`] several at once
//! as one step (a paste, all or nothing). The document applies a new edit
//! ([`Document::set_cell`], [`Document::set_cells`]) and hands back its
//! command; undoing it is applying the command's
//! [`inverse`](Command::inverse), and redoing it is applying it again
//! ([`Document::apply`]). A command applies only to the edits it was made
//! in (its [`Lineage`]), and only if each cell still holds its old value.
//!
//! The core keeps no undo stack: the app's `NSUndoManager` holds the
//! commands (task 2.5). For recovery the app also keeps its own journal of
//! the commands applied, since an undo manager can't be listed: after a
//! failure it opens the file afresh and replays the journal
//! ([`Document::replay`], ADR-0008 decision 5), which reports the commands
//! that no longer apply instead of guessing.
//!
//! **Missing cells** (ADR-0005 decision 2). A cell past the end of a short
//! or blank row (a hatched cell) can be edited: the row then reads as long
//! as its last edited cell, with empty cells in between, and saving appends
//! the delimiters and the value at the end of the row (task 2.2). A missing
//! cell's value is `None`, not `""`: setting one to `""` is no edit, and
//! setting an edited one back to `""` makes it missing again, so the row's
//! bytes come back. Edits past an unterminated quote are refused (ADR-0004
//! decision 8).
//!
//! **Logical coordinates.** In 2.1 a logical row is a physical row (the
//! header row, if any, is row 0) and a logical column is a field of that
//! row, or a cell past its end. Task 2.4 adds row and column inserts and
//! deletes; a command then names the row and column as they are when it is
//! applied. Undo applies the inverses in reverse order, so each one meets
//! the coordinates its command was made in.
//!
//! **The overlay** (`overlay.rs`, crate-private) maps each edited physical
//! row to its edited cells, plus what the diagnostics need to know about
//! the row as it reads now. Every reader of a row asks the overlay for that
//! row, which is one look-up in a `BTreeMap` (O(log n) in the edited rows),
//! and nothing at all when there are no edits. The readers see a row
//! through a `RowView` (in `document`), which is where task 2.4's piece
//! list and column map will plug in.
//!
//! [`Document::set_cell`]: crate::document::Document::set_cell
//! [`Document::set_cells`]: crate::document::Document::set_cells
//! [`Document::apply`]: crate::document::Document::apply
//! [`Document::replay`]: crate::document::Document::replay

mod command;
mod overlay;

pub use command::{COLUMN_LIMIT, CellChange, Command, Edit, EditError, Lineage, Replay};
pub(crate) use overlay::{EditStore, Kinds, Overlay, RowEdits};
