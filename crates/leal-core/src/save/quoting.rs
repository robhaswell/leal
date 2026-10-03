#![cfg_attr(not(test), expect(dead_code, reason = "task 2.4c's writer"))]
//! Per-column quoting of new fields (ADR-0004 decision 2, ADR-0005
//! decision 3, ADR-0014 decision 4; task 2.4b), for the writer (task 2.4c).
//!
//! A new field (an inserted row's, or a column insert's) is quoted if it
//! needs it, if the file quotes every field, or if its column quotes every
//! field: the column has at least one non-empty field and every non-empty
//! field in it is quoted. The column is the logical column as the document
//! is now; its fields are the original fields at that position, edited
//! ones judged by their original bytes, in rows that aren't original blank
//! lines; new cells (inserted ones, hatched ones, an inserted row's) don't
//! count. An empty field is one with no bytes (`""` is not empty). Hatched
//! cells keep task 2.2's rule: quoted if needed, or if the file quotes
//! every field.
//!
//! The census takes one pass over the document's rows, each through its
//! `RowView` ([`ColumnQuoting::add`]), with the file's every-field check
//! (ADR-0004 decision 1) in the same pass ([`ColumnQuoting::add_file_row`]):
//! [`ColumnQuoting::census`] makes it (in `document`, which reads the rows).

use crate::document::RowView;

/// The census of which columns quote every field ([`quoted`](Self::quoted)).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ColumnQuoting {
    /// Per logical column: (a non-empty original field seen, every one
    /// seen quoted).
    columns: Vec<(bool, bool)>,
    /// Every field of every non-blank row of the file is quoted, so far.
    every_field: bool,
    /// A non-blank row of the file has been seen.
    any_row: bool,
}

impl ColumnQuoting {
    /// An empty census: no column quotes every field yet.
    pub(crate) fn new() -> ColumnQuoting {
        ColumnQuoting {
            columns: Vec::new(),
            every_field: true,
            any_row: false,
        }
    }

    /// Counts a row of the document as it reads now: its own fields, by
    /// where they are now, unless it is a blank line of the file.
    pub(crate) fn add(&mut self, view: &RowView<'_>) {
        if view.is_blank_line() {
            return;
        }
        for (column, cell) in view.fields_now() {
            if cell.is_empty() {
                continue;
            }
            if self.columns.len() <= column {
                self.columns.resize(column + 1, (false, true));
            }
            let entry = &mut self.columns[column];
            entry.0 = true;
            entry.1 &= cell.quoted();
        }
    }

    /// Counts a row of the file, as read (deleted or not), for whether the
    /// file quotes every field (ADR-0004 decision 1).
    pub(crate) fn add_file_row(&mut self, view: &RowView<'_>) {
        let Some(row) = view.file_fields() else {
            return;
        };
        if row.len() == 1 && row[0].is_empty() && !row[0].quoted() {
            return; // a blank line
        }
        self.any_row = true;
        self.every_field &= row.iter().all(|field| field.quoted());
    }

    /// Whether the file quotes every field.
    pub(crate) fn every_field(&self) -> bool {
        self.any_row && self.every_field
    }

    /// Whether a new field in logical column `column` is quoted, besides
    /// when its value needs it: the file quotes every field, or the
    /// column does.
    pub(crate) fn quoted(&self, column: usize) -> bool {
        self.every_field()
            || self
                .columns
                .get(column)
                .is_some_and(|&(seen, quoted)| seen && quoted)
    }
}
