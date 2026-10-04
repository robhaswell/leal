//! Paste and Clear (task 2.6, DESIGN §4.2): several cells changed as one
//! command, a cell edit of each ([`Edit::SetCells`]), so the overlay,
//! undo, redo, the journal's replay and saving treat them as they treat
//! typing: only the cells changed are written, and every other byte is the
//! file's.
//!
//! Like row and column edits they wait for the whole file to be read and
//! for a save to end (ADR-0014 decision 1, as the app asks for 2.6), take
//! at most [`CELL_BATCH_LIMIT`] cells, and never add rows or columns: a
//! paste that runs past the last row or column is refused.

use std::ops::Range;
use std::sync::Arc;

use super::editing::Target;
use super::{Document, Reading};
use crate::edit::{
    CELL_BATCH_LIMIT, Command, Edit, EditError, PASTE_BYTE_LIMIT, Pasted, parse_tsv,
};

impl Document {
    /// Pastes clipboard `text` (read as tab-separated values,
    /// [`parse_tsv`]) into the selection of logical rows `rows` and columns
    /// `columns`, as one command for the undo history. One value goes
    /// into every selected cell; more go in as a block from the
    /// selection's top-left cell (once, whatever the selection's size),
    /// each row of the text changing only as many cells as it has. A value
    /// pasted into a cell past the end of its row (a hatched cell) pads the
    /// row as typing does, and `""` there is no edit (ADR-0005 decision 2).
    /// `columns_shown` is how many columns the grid shows: the block must
    /// fit within them, and within the rows. `None` if no cell changes.
    ///
    /// It reads each row it changes on the caller's thread, so it takes at
    /// most [`CELL_BATCH_LIMIT`] cells and [`PASTE_BYTE_LIMIT`] bytes.
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] or [`EditError::Saving`] (as for
    /// [`can_paste`](Self::can_paste)); [`EditError::TooMuchText`],
    /// [`EditError::TooManyCells`]; [`EditError::PastLastRow`] or
    /// [`EditError::PastLastColumn`] for a block that doesn't fit;
    /// [`EditError::NoSuchRow`] for a selection past the end; and those of
    /// [`set_cells`](Self::set_cells), such as
    /// [`EditError::AfterUnterminatedQuote`] (ADR-0004 decision 8). The
    /// document is then unchanged.
    pub fn paste(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
        columns_shown: usize,
        text: &str,
    ) -> Result<Option<Command>, EditError> {
        if rows.is_empty() || columns.is_empty() {
            return Ok(None);
        }
        self.change_rows(|reading| {
            let pasted = pasted(text)?;
            let row_count = Self::logical_rows(reading);
            let targets = if let Some(value) = pasted.single() {
                let cells = cells_in(&rows, &columns)?;
                let bytes = value.len().saturating_mul(cells);
                if bytes > PASTE_BYTE_LIMIT {
                    return Err(EditError::TooMuchText { bytes });
                }
                if rows.end > row_count {
                    return Err(EditError::NoSuchRow { row: row_count });
                }
                fill(&rows, &columns, value)
            } else {
                let (top, left) = (rows.start, columns.start);
                if top.saturating_add(pasted.height()) > row_count {
                    return Err(EditError::PastLastRow {
                        rows: pasted.height(),
                    });
                }
                if left.saturating_add(pasted.width()) > columns_shown {
                    return Err(EditError::PastLastColumn {
                        columns: pasted.width(),
                    });
                }
                block(&pasted, top, left)
            };
            set_all(reading, &targets)
        })
    }

    /// Whether Paste can run now, for the app to enable it: as for
    /// [`can_change_rows`](Self::can_change_rows). What is pasted is
    /// checked when it is pasted.
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] or [`EditError::Saving`].
    pub fn can_paste(&self) -> Result<(), EditError> {
        self.can_change_rows()
    }

    /// Clears the selection of logical rows `rows` and columns `columns`
    /// (Delete, task 2.6): each cell set to `""`, as one command. A cell
    /// past the end of its row (a hatched cell) stays missing, and one
    /// edited there goes back to missing, so the row's bytes come back
    /// (ADR-0005 decision 2). `None` if no cell changes (they were all
    /// empty).
    ///
    /// # Errors
    ///
    /// As for [`can_clear_cells`](Self::can_clear_cells), and those of
    /// [`set_cells`](Self::set_cells). The document is then unchanged.
    pub fn clear_cells(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
    ) -> Result<Option<Command>, EditError> {
        if rows.is_empty() || columns.is_empty() {
            return Ok(None);
        }
        self.change_rows(|reading| {
            check_clear(reading, &rows, &columns)?;
            set_all(reading, &fill(&rows, &columns, ""))
        })
    }

    /// Whether the selection of logical rows `rows` and columns `columns`
    /// can be cleared now, for the app to enable Delete. It reads no row.
    ///
    /// # Errors
    ///
    /// [`EditError::StillReading`] or [`EditError::Saving`] (as for
    /// [`can_change_rows`](Self::can_change_rows)),
    /// [`EditError::TooManyCells`] past [`CELL_BATCH_LIMIT`], and
    /// [`EditError::NoSuchRow`] for rows past the end or none.
    pub fn can_clear_cells(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
    ) -> Result<(), EditError> {
        self.can_change_rows()?;
        check_clear(&self.current(), &rows, &columns)
    }
}

/// `text` as cells, within the limits.
fn pasted(text: &str) -> Result<Pasted<'_>, EditError> {
    if text.len() > PASTE_BYTE_LIMIT {
        return Err(EditError::TooMuchText { bytes: text.len() });
    }
    parse_tsv(text, CELL_BATCH_LIMIT).map_err(|count| EditError::TooManyCells { count })
}

/// How many cells `rows` × `columns` is, within [`CELL_BATCH_LIMIT`].
fn cells_in(rows: &Range<usize>, columns: &Range<usize>) -> Result<usize, EditError> {
    let count = rows.len().saturating_mul(columns.len());
    if count > CELL_BATCH_LIMIT {
        return Err(EditError::TooManyCells { count });
    }
    Ok(count)
}

/// Whether `rows` × `columns` can be cleared in `reading`.
fn check_clear(
    reading: &Reading,
    rows: &Range<usize>,
    columns: &Range<usize>,
) -> Result<(), EditError> {
    if rows.is_empty() || columns.is_empty() {
        return Err(EditError::NoSuchRow { row: rows.start });
    }
    cells_in(rows, columns)?;
    let row_count = Document::logical_rows(reading);
    if rows.end > row_count {
        return Err(EditError::NoSuchRow {
            row: rows.start.max(row_count),
        });
    }
    Ok(())
}

/// `value` in each cell of `rows` × `columns`, row by row.
fn fill<'a>(rows: &Range<usize>, columns: &Range<usize>, value: &'a str) -> Vec<Target<'a>> {
    rows.clone()
        .flat_map(|row| {
            columns.clone().map(move |column| Target {
                row,
                column,
                value: Some(value),
                expected: None,
            })
        })
        .collect()
}

/// `pasted`'s cells from row `top`, column `left`.
fn block<'a>(pasted: &'a Pasted<'_>, top: usize, left: usize) -> Vec<Target<'a>> {
    pasted
        .rows()
        .iter()
        .enumerate()
        .flat_map(|(r, cells)| {
            cells.iter().enumerate().map(move |(c, value)| Target {
                row: top + r,
                column: left + c,
                value: Some(value.as_ref()),
                expected: None,
            })
        })
        .collect()
}

/// Sets `targets` in `reading`, all or none, as one command (`None` if no
/// cell changes). The caller holds the writer lock (`change_rows`).
fn set_all(reading: &Arc<Reading>, targets: &[Target<'_>]) -> Result<Option<Command>, EditError> {
    let (lineage, changes) = Document::change_in(reading, None, targets, false)?;
    Ok((!changes.is_empty()).then_some(Command {
        lineage,
        edit: Edit::SetCells(changes),
    }))
}
