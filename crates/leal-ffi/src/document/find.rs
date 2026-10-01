//! Find, Copy and the cell inspector, for Swift (task 1.8). See
//! `leal_core::document::Search`, `Document::copy_cells` and
//! `Document::cell_value`.
//!
//! - [`Document::find`] starts a [`Search`], a P2 job; the search's
//!   methods say what it has found so far.
//! - [`Document::copy_cells`] starts a [`CopyJob`]: await its
//!   [`job`](CopyJob::job), then [`take`](CopyJob::take) the text.
//! - [`Document::cell_value`]: one cell's whole value, for the inspector.
//!
//! Rows here are physical rows (the header row, if any, is row 0), as
//! everywhere in this crate. A search and a copy belong to their document:
//! once it has failed (DESIGN §3.9), their calls fail too, and a panic in
//! one of their jobs fails it.

use std::sync::Arc;

use leal_core::document;
use leal_core::find::Query;

#[cfg(test)]
mod tests;

use super::{Document, Failure, Job, guarded, read_error, to_index, to_u32, to_u64, to_usize};
use crate::LealError;

/// Where a search has got to. See `leal_core::document::SearchProgress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SearchProgress {
    /// The reading searched (`FirstScreen.generation`).
    pub generation: u64,
    /// Matching cells found so far.
    pub matches: u64,
    /// Every physical row before this one has been searched.
    pub rows_searched: u64,
    /// Every row has been searched.
    pub complete: bool,
}

/// The answer to **Next** or **Previous** ([`Search::step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchStep {
    /// The match to select: its physical row and field, its 1-based number
    /// among the matches ("k of N"), and whether the step went round the
    /// end of the file.
    Found {
        /// The physical row.
        row: u64,
        /// The field.
        column: u32,
        /// The match's number, from 1.
        ordinal: u64,
        /// Past the last match to the first, or before the first to the
        /// last.
        wrapped: bool,
    },
    /// The search hasn't got that far yet: ask again when it has found
    /// more or finished.
    Pending,
    /// Nothing matches.
    NotFound,
}

/// A stretch of a cell's text, in UTF-16 code units (`NSRange`'s units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TextRange {
    /// Where it starts.
    pub start: u32,
    /// How long it is.
    pub length: u32,
}

/// A matching cell in a window of the grid ([`Search::matches_in`]).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CellMatch {
    /// The physical row.
    pub row: u64,
    /// The field.
    pub column: u32,
    /// Where the query is in the start of the cell's text the grid shows.
    /// Empty if the only matches are further on.
    pub ranges: Vec<TextRange>,
}

/// One cell's whole value, for the cell inspector. See
/// `leal_core::document::CellValue`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CellValue {
    /// The display value, or its first `max_chars` characters.
    pub text: String,
    /// The value is longer than `text`.
    pub truncated: bool,
    /// The whole value's length, in characters (Unicode scalar values).
    pub characters: u64,
    /// Line breaks plus one; 0 for an empty value.
    pub lines: u64,
    /// The value has bytes that aren't valid text, shown as U+FFFD.
    pub invalid: bool,
    /// The row has this field: false for a short row's missing cell.
    pub exists: bool,
}

/// A running or finished search for the find bar. Releasing it cancels it.
#[derive(Debug, uniffi::Object)]
pub struct Search {
    search: document::Search,
    path: String,
    failure: Arc<Failure>,
}

/// Copying cells as tab-separated text, in the background.
#[derive(Debug, uniffi::Object)]
pub struct CopyJob {
    job: leal_core::schedule::JobHandle<document::CopiedText>,
    path: String,
    failure: Arc<Failure>,
}

#[uniffi::export]
impl Document {
    /// Starts searching for `text` in every cell's display value, case
    /// sensitive or not, as a P2 job: it searches the rows indexed so far
    /// and keeps up as more arrive, and pauses while the user interacts.
    /// `None` for an empty query, or one too long to search for.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn find(
        &self,
        text: String,
        case_sensitive: bool,
    ) -> Result<Option<Arc<Search>>, LealError> {
        self.call(|| {
            let query = Query {
                text,
                case_sensitive,
            };
            let Ok(search) = self.document.find(&query) else {
                return Ok(None);
            };
            self.failure.watch(search.job().control());
            Ok(Some(Arc::new(Search {
                search,
                path: self.path.clone(),
                failure: Arc::clone(&self.failure),
            })))
        })
    }

    /// Starts copying physical rows `row_start` to `row_start + row_count`
    /// and fields `column_start` to `column_start + column_count` as
    /// tab-separated display values: cells with a tab, line break or quote
    /// are quoted. Rows past the indexed ones are waited for, and rows past
    /// the end of the file left out.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn copy_cells(
        &self,
        row_start: u64,
        row_count: u64,
        column_start: u32,
        column_count: u32,
    ) -> Result<Arc<CopyJob>, LealError> {
        self.call(|| {
            let (rows, columns) = copy_ranges(row_start, row_count, column_start, column_count);
            let job = self.document.copy_cells(rows, columns);
            self.failure.watch(job.control());
            Ok(Arc::new(CopyJob {
                job,
                path: self.path.clone(),
                failure: Arc::clone(&self.failure),
            }))
        })
    }

    /// Copies physical rows `row_start` to `row_start + row_count` and
    /// fields `column_start` to `column_start + column_count` as
    /// tab-separated display values at once, if the index has every one of
    /// the rows (or is complete); `None` otherwise, when
    /// [`copy_cells`](Self::copy_cells) waits for them. For a selection of
    /// a few screens; the app estimates first
    /// ([`estimated_copy_bytes`](Self::estimated_copy_bytes)).
    ///
    /// # Errors
    ///
    /// As for [`rows`](Document::rows).
    pub fn copy_cells_now(
        &self,
        row_start: u64,
        row_count: u64,
        column_start: u32,
        column_count: u32,
    ) -> Result<Option<String>, LealError> {
        self.call(|| {
            let (rows, columns) = copy_ranges(row_start, row_count, column_start, column_count);
            self.document
                .copy_cells_now(rows, columns)
                .map_err(|error| read_error(&self.path, &error))
        })
    }

    /// About how many bytes copying those cells would put on the
    /// clipboard, without reading them, so the app can ask before a very
    /// large copy.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn estimated_copy_bytes(
        &self,
        row_start: u64,
        row_count: u64,
        column_start: u32,
        column_count: u32,
    ) -> Result<u64, LealError> {
        self.call(|| {
            let (rows, columns) = copy_ranges(row_start, row_count, column_start, column_count);
            Ok(self.document.estimated_copy_bytes(rows, columns))
        })
    }

    /// Field `column` of physical row `row` in full, up to `max_chars`
    /// characters of it, with its length in characters and lines; `None`
    /// if the row can't be read yet. It decodes the whole value: call it
    /// off the main thread.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Document::rows).
    pub fn cell_value(
        &self,
        row: u64,
        column: u32,
        max_chars: u32,
    ) -> Result<Option<CellValue>, LealError> {
        self.call(|| {
            let value = self
                .document
                .cell_value(to_index(row), to_usize(column), to_usize(max_chars))
                .map_err(|error| read_error(&self.path, &error))?;
            Ok(value.map(|value| CellValue {
                text: value.text,
                truncated: value.truncated,
                characters: to_u64(value.characters),
                lines: to_u64(value.lines),
                invalid: value.invalid,
                exists: value.exists,
            }))
        })
    }
}

#[uniffi::export]
impl Search {
    /// The search's job: await it (through `withTaskCancellationHandler`),
    /// or cancel it.
    #[must_use]
    pub fn job(&self) -> Arc<Job> {
        Arc::new(Job {
            control: self.search.job().control().clone(),
        })
    }

    /// Stops the search within one chunk. What it found is kept.
    pub fn cancel(&self) {
        self.search.cancel();
    }

    /// Where the search has got to.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn progress(&self) -> Result<SearchProgress, LealError> {
        self.call(|| {
            let progress = self.search.progress();
            Ok(SearchProgress {
                generation: progress.generation,
                matches: progress.matches,
                rows_searched: to_u64(progress.rows_searched),
                complete: progress.complete,
            })
        })
    }

    /// **Next** (`forward`) or **Previous** from physical row `row`, field
    /// `column` (any cell, a match or not), or with no `row` the first or
    /// last match. It wraps round the ends once the search is complete.
    ///
    /// # Errors
    ///
    /// As for [`Document::rows`]: a matching row is read again.
    pub fn step(
        &self,
        row: Option<u64>,
        column: u32,
        forward: bool,
    ) -> Result<SearchStep, LealError> {
        self.call(|| {
            let from = row.map(|row| document::Place {
                row: to_index(row),
                column: to_usize(column),
            });
            let step = self
                .search
                .step(from, forward)
                .map_err(|error| read_error(&self.path, &error))?;
            Ok(match step {
                document::SearchStep::Found {
                    place,
                    ordinal,
                    wrapped,
                } => SearchStep::Found {
                    row: to_u64(place.row),
                    column: to_u32(place.column),
                    ordinal,
                    wrapped,
                },
                document::SearchStep::Pending => SearchStep::Pending,
                document::SearchStep::NotFound => SearchStep::NotFound,
            })
        })
    }

    /// The 1-based number of the match at physical row `row`, field
    /// `column` ("k of N"), or `None` if that cell isn't one found so far.
    ///
    /// # Errors
    ///
    /// As for [`step`](Self::step).
    pub fn ordinal(&self, row: u64, column: u32) -> Result<Option<u64>, LealError> {
        self.call(|| {
            self.search
                .ordinal(document::Place {
                    row: to_index(row),
                    column: to_usize(column),
                })
                .map_err(|error| read_error(&self.path, &error))
        })
    }

    /// The matches found so far in physical rows `row_start` to `row_start
    /// + row_count` and fields `column_start` to `column_start +
    /// column_count`, with where the query is in the first `max_chars`
    /// characters of each: one call per tile of the grid.
    ///
    /// # Errors
    ///
    /// As for [`step`](Self::step).
    pub fn matches_in(
        &self,
        row_start: u64,
        row_count: u32,
        column_start: u32,
        column_count: u32,
        max_chars: u32,
    ) -> Result<Vec<CellMatch>, LealError> {
        self.call(|| {
            let start = to_index(row_start);
            let rows = start..start.saturating_add(to_usize(row_count));
            let first = to_usize(column_start);
            let columns = first..first.saturating_add(to_usize(column_count));
            let matches = self
                .search
                .matches_in(rows, columns, to_usize(max_chars))
                .map_err(|error| read_error(&self.path, &error))?;
            Ok(matches
                .into_iter()
                .map(|found| CellMatch {
                    row: to_u64(found.row),
                    column: to_u32(found.column),
                    ranges: found
                        .ranges
                        .into_iter()
                        .map(|range| TextRange {
                            start: to_u32(range.start),
                            length: to_u32(range.len()),
                        })
                        .collect(),
                })
                .collect())
        })
    }
}

/// A copy's rows and fields as ranges, saturating.
fn copy_ranges(
    row_start: u64,
    row_count: u64,
    column_start: u32,
    column_count: u32,
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let start = to_index(row_start);
    let first = to_usize(column_start);
    (
        start..start.saturating_add(to_index(row_count)),
        first..first.saturating_add(to_usize(column_count)),
    )
}

impl Search {
    fn call<T>(&self, call: impl FnOnce() -> Result<T, LealError>) -> Result<T, LealError> {
        guarded(&self.failure, &self.path, call)
    }
}

#[uniffi::export]
impl CopyJob {
    /// The copy's job: await it, then [`take`](Self::take) the text.
    #[must_use]
    pub fn job(&self) -> Arc<Job> {
        Arc::new(Job {
            control: self.job.control().clone(),
        })
    }

    /// Stops the copy.
    pub fn cancel(&self) {
        self.job.cancel();
    }

    /// The copied text, once the job has finished, the first time it is
    /// asked for: `None` before, after, or if the job failed.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn take(&self) -> Result<Option<String>, LealError> {
        guarded(&self.failure, &self.path, || {
            Ok(match self.job.result() {
                Some(Ok(text)) => text.take(),
                Some(Err(_)) | None => None,
            })
        })
    }
}

#[uniffi::export]
impl CopyJob {
    /// [`take`](Self::take), after waiting up to `timeout_ms` for the job
    /// to finish. This blocks the calling thread: it is for a pasteboard
    /// asking for the promised text, which must be given there and then.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn take_waiting(&self, timeout_ms: u32) -> Result<Option<String>, LealError> {
        let _ = self
            .job
            .control()
            .wait_timeout(std::time::Duration::from_millis(u64::from(timeout_ms)));
        self.take()
    }
}

impl Drop for CopyJob {
    fn drop(&mut self) {
        self.job.cancel();
    }
}
