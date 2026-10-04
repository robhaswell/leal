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
//! **Logical coordinates.** A logical row is a row as the document has it
//! now (the header row, if any, is row 0) and a logical column is a field
//! of that row, or a cell past its end. Rows can be inserted and deleted
//! (task 2.4a: [`Edit::InsertRows`], [`Edit::DeleteRows`]); a command names
//! the rows as they are when it is applied. Undo applies the inverses in
//! reverse order, so each one meets the coordinates its command was made
//! in.
//!
//! **Row ids and the piece list** (`rows.rs`, `docs/tasks/2.4.md` §1).
//! Every row has a [`RowId`] for life, and a `RowMap` lists which row each
//! logical row is: stretches of the file's rows and runs of inserted rows.
//! With no row inserted or deleted it is the identity and costs nothing.
//! Row inserts and deletes need the whole file read, and no save running
//! (ADR-0014 decision 1). A delete keeps the rows it took out, with their
//! cell edits, in its command ([`Rows`]), so its undo puts back the same
//! rows and their original bytes; in other edits (after a save, or in a
//! replay) a row command works by value instead (ADR-0014 decision 3).
//!
//! **Columns** (`columns.rs`, task 2.4b, `docs/tasks/2.4.md` §3). Every
//! cell has an identity for life (`CellId`: one of the row's own fields,
//! the cell a column insert gave it, or a hatched cell), and edits are kept
//! by it, so a column insert ([`Edit::InsertColumn`]) or delete
//! ([`Edit::DeleteColumn`]) moves cells without rewriting any edit. Which
//! cell each logical column of a row is, is its layout: by default, its own
//! fields with each operation applied where oracle rule 6 says (ADR-0004
//! decision 5), which depends only on its field count; an edited row the
//! rule decides otherwise for keeps its layout written out. So an operation
//! costs a look at each edited row, never one per row. Column inserts and
//! deletes need the whole file read, and restart Find (ADR-0014 decisions
//! 1 and 2); their undo works as for rows, by identity in the same edits,
//! by value in others.
//!
//! **The overlay** (`overlay.rs`, crate-private) maps each edited row, by
//! id, to its edited cells, plus what the diagnostics need to know about
//! the row as it reads now; it also holds the inserted rows and the piece
//! list. Every reader of a row asks the overlay for that row, which is one
//! look-up in a `BTreeMap` (O(log n) in the edited rows), and nothing at
//! all when there are no edits. The readers see a row through a `RowView`
//! (in `document`), and walk logical rows through the piece list's
//! segments.
//!
//! [`Document::set_cell`]: crate::document::Document::set_cell
//! [`Document::set_cells`]: crate::document::Document::set_cells
//! [`Document::apply`]: crate::document::Document::apply
//! [`Document::replay`]: crate::document::Document::replay

mod columns;
mod command;
mod overlay;
mod rows;
mod structural;
mod value;

pub(crate) use columns::{
    CellId, ColumnOp, Columns, Fold, Layout, OpId, OpKind, Own, Parts, TABLE, fresh_appended,
};
pub use command::{COLUMN_LIMIT, CellChange, Command, Edit, EditError, Lineage, Replay};
pub(crate) use overlay::{
    ColumnChange, EditStore, InsertedRow, Kinds, Overlay, OverlayRow, RowChange, RowEdits,
};
pub use rows::RowId;
pub(crate) use rows::{Piece, RowMap, Segment, Slot};
pub(crate) use structural::{Changed, ColumnSource, RowSource};
pub use structural::{Column, Rows};
pub(crate) use value::{RawField, ReadAs, Value};
