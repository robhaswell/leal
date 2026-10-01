//! **Find** over a whole document (task 1.8): a background search that
//! streams the cells it finds, and the answers the find bar needs from it.
//!
//! [`Document::find`] starts a [`Search`] as a P2 job (DESIGN §3.10): it
//! runs on the background pool, works in chunks of about 64 KiB of rows
//! (well under rule 3's 5 ms), pauses at each chunk while the user scrolls
//! or types, and stops within a chunk when cancelled. It searches the rows
//! indexed so far and keeps up as more arrive, so it works while indexing
//! (rule 6's promise for filters, kept for find too), and ends when it has
//! searched the last row.
//!
//! **A match is a cell** whose display value holds the query
//! ([`Matcher`]). The count and "k of N" count cells, in file order: row
//! by row, and left to right within a row. The header row isn't searched:
//! it isn't one of the grid's rows.
//!
//! **What is kept.** For each row with a match, its row number and the
//! running count of matching cells up to and including it: 12 bytes a
//! matching row, so a query that matches every row of the 1M-row reference
//! file keeps 12 MB. Which cells of a row match is worked out again when
//! asked (one row's parse), for the few rows on screen and for **Next**.
//!
//! **Raw bytes first.** For UTF-8, the matcher can search the file's bytes
//! for the query ([`Matcher::raw_candidates`]); only rows where it shows up
//! are split into fields, and only fields where it shows up are checked
//! value by value. Otherwise (another encoding, or a query with U+FFFD)
//! every value is checked.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Duration;

use super::{Document, Place, Reading};
use crate::find::{FindError, Matcher, Query};
use crate::index::Status;
use crate::schedule::{Interval, Job, JobError, JobHandle, Priority};
use crate::source::{ReadError, Source};

/// About how many bytes of rows one chunk of a search covers.
pub const SEARCH_CHUNK_BYTES: usize = 64 << 10;

/// How long a search (or a copy) waits for the index to publish more rows
/// once it has caught up with it.
pub(super) const WAIT_FOR_INDEX: Duration = Duration::from_millis(1);

/// What a finished search found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchSummary {
    /// Matching cells.
    pub matches: u64,
    /// Rows with at least one.
    pub rows: usize,
}

/// Where a search has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchProgress {
    /// The reading searched ([`Document::generation`]).
    pub generation: u64,
    /// Matching cells found so far.
    pub matches: u64,
    /// Rows searched so far: every physical row before this one.
    pub rows_searched: usize,
    /// Whether every row has been searched.
    pub complete: bool,
}

/// The answer to **Next** or **Previous**.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStep {
    /// The match to select, its place in the count ("k of N", from 1), and
    /// whether the step went round the end of the file to get there.
    Found {
        /// The cell.
        place: Place,
        /// Its 1-based number among the matches.
        ordinal: u64,
        /// The step wrapped round (past the last match to the first, or
        /// before the first to the last).
        wrapped: bool,
    },
    /// The search hasn't got far enough to say yet: ask again when it has
    /// found more, or finished.
    Pending,
    /// Nothing matches.
    NotFound,
}

/// One matching cell in a window of the grid ([`Search::matches_in`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellMatch {
    /// The physical row.
    pub row: usize,
    /// The field.
    pub column: usize,
    /// Where the query is in the start of the value the grid shows, in
    /// UTF-16 code units ([`Matcher::utf16_ranges`]). Empty if the only
    /// matches are further on than that.
    pub ranges: Vec<Range<usize>>,
}

/// A running (or finished) search ([`Document::find`]). Dropping it
/// cancels the search.
///
/// It runs on the background pool as a P2 job (DESIGN §3.10), in chunks of
/// about [`SEARCH_CHUNK_BYTES`] of rows, well under rule 3's 5 ms, and
/// pauses at each chunk while the user scrolls or types. It searches the
/// rows indexed so far and keeps up as more arrive, so it works while
/// indexing, and ends once it has searched the last row. A match is a cell
/// whose display value holds the query ([`Matcher`]); the header row isn't
/// searched.
pub struct Search {
    state: Arc<SearchState>,
    job: JobHandle<SearchSummary>,
}

/// What the search's job and its readers share.
struct SearchState {
    reading: Arc<Reading>,
    source: Arc<Source>,
    head: Arc<[u8]>,
    matcher: Matcher,
    /// The first row searched: 1 if the file has a header row.
    first_row: usize,
    found: Mutex<Found>,
    /// Run once by the next step, between its look at the matches and its
    /// look at whether the search is complete: a test makes the search
    /// finish there.
    #[cfg(test)]
    before_settling: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// The matches found so far.
#[derive(Debug, Default)]
struct Found {
    /// Rows with a match, in order.
    rows: Vec<u32>,
    /// `ends[i]`: matching cells in `rows[..=i]`.
    ends: Vec<u64>,
    /// Every row before this one has been searched.
    searched: usize,
    complete: bool,
}

impl Found {
    fn total(&self) -> u64 {
        self.ends.last().copied().unwrap_or(0)
    }

    /// Matching cells before `rows[i]`.
    fn before(&self, i: usize) -> u64 {
        i.checked_sub(1).map_or(0, |prev| self.ends[prev])
    }

    fn row(&self, i: usize) -> usize {
        to_usize(self.rows[i])
    }
}

impl Document {
    /// Starts searching the current reading for `query` (the find bar,
    /// task 1.8), as a P2 job (see [`Search`]). It returns at once; the
    /// [`Search`] gives what it has found so far.
    ///
    /// # Errors
    ///
    /// The [`FindError`] for an empty or too long query.
    pub fn find(&self, query: &Query) -> Result<Search, FindError> {
        let reading = self.current();
        let matcher = Matcher::new(query, reading.detection.encoding)?;
        let first_row = usize::from(reading.detection.header);
        let state = Arc::new(SearchState {
            reading,
            source: Arc::clone(&self.source),
            head: Arc::clone(&self.head),
            matcher,
            first_row,
            found: Mutex::new(Found {
                searched: first_row,
                ..Found::default()
            }),
            #[cfg(test)]
            before_settling: Mutex::default(),
        });
        let worker = Arc::clone(&state);
        let job = self
            .scheduler
            .spawn(Priority::P2, Interval::Find, move |job| worker.run(job));
        Ok(Search { state, job })
    }
}

impl Search {
    /// The search's job: to wait for it, or cancel it.
    #[must_use]
    pub fn job(&self) -> &JobHandle<SearchSummary> {
        &self.job
    }

    /// Stops the search within one chunk. What it found is kept.
    pub fn cancel(&self) {
        self.job.cancel();
    }

    /// The reading searched.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state.reading.generation
    }

    /// Where the search has got to.
    #[must_use]
    pub fn progress(&self) -> SearchProgress {
        let found = self.state.lock();
        SearchProgress {
            generation: self.state.reading.generation,
            matches: found.total(),
            rows_searched: found.searched,
            complete: found.complete,
        }
    }

    /// **Next** (`forward`) or **Previous** from the cell `from` (a
    /// physical row and a field), which need not be a match: the first
    /// match after it, or the last one before it, in file order. With no
    /// `from`, the first match (forward) or the last one. Past the last
    /// match it wraps round to the first, and before the first to the last,
    /// once the search is complete; until then it is
    /// [`SearchStep::Pending`].
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if a matching row can't be read again (a removable
    /// drive that vanished).
    pub fn step(&self, from: Option<Place>, forward: bool) -> Result<SearchStep, ReadError> {
        if forward {
            self.step_forward(from)
        } else {
            self.step_backward(from)
        }
    }

    fn step_forward(&self, from: Option<Place>) -> Result<SearchStep, ReadError> {
        let start = from.map_or(0, |place| place.row);
        loop {
            // The first matching row at or after `from`'s, among the
            // matches found by now.
            let mut i = self
                .state
                .lock()
                .rows
                .partition_point(|&row| to_usize(row) < start);
            while let Some((row, before)) = self.match_row(i) {
                let columns = self.state.columns_of(row)?;
                let next = match from {
                    Some(place) if place.row == row => {
                        columns.iter().position(|&column| column > place.column)
                    }
                    _ => (!columns.is_empty()).then_some(0),
                };
                if let Some(rank) = next {
                    return Ok(found(row, columns[rank], before, rank, false));
                }
                i += 1;
            }
            self.state.before_settling();
            let (complete, first) = {
                let found = self.state.lock();
                // A chunk may have landed since the last look, and the
                // search may have finished with it: its rows come first.
                if found.rows.len() > i {
                    continue;
                }
                (
                    found.complete,
                    (!found.rows.is_empty()).then(|| found.row(0)),
                )
            };
            return match (complete, first) {
                (false, _) => Ok(SearchStep::Pending),
                (true, None) => Ok(SearchStep::NotFound),
                (true, Some(row)) => {
                    let columns = self.state.columns_of(row)?;
                    Ok(match columns.first() {
                        Some(&column) => found(row, column, 0, 0, true),
                        None => SearchStep::NotFound,
                    })
                }
            };
        }
    }

    fn step_backward(&self, from: Option<Place>) -> Result<SearchStep, ReadError> {
        // Rows between the last searched one and `from` may still hold the
        // previous match, so it is only looked for once they are searched.
        let settled =
            |found: &Found| found.complete || from.is_some_and(|place| place.row < found.searched);
        loop {
            let (mut i, was_settled) = {
                let found = self.state.lock();
                let i = match from {
                    Some(place) => found
                        .rows
                        .partition_point(|&row| to_usize(row) <= place.row),
                    None => found.rows.len(),
                };
                (i, settled(&found))
            };
            if was_settled {
                while let Some(j) = i.checked_sub(1) {
                    let Some((row, before)) = self.match_row(j) else {
                        break;
                    };
                    let columns = self.state.columns_of(row)?;
                    let previous = match from {
                        Some(place) if place.row == row => {
                            columns.iter().rposition(|&column| column < place.column)
                        }
                        _ => columns.len().checked_sub(1),
                    };
                    if let Some(rank) = previous {
                        return Ok(found(row, columns[rank], before, rank, false));
                    }
                    i = j;
                }
            }
            self.state.before_settling();
            let (complete, last) = {
                let found = self.state.lock();
                // The search got far enough (or finished) since the first
                // look: look again, now that the rows before `from` are in.
                if !was_settled && settled(&found) {
                    continue;
                }
                let last = found.rows.len().checked_sub(1);
                (
                    found.complete,
                    last.map(|i| (found.row(i), found.before(i))),
                )
            };
            return match (complete, last) {
                (false, _) => Ok(SearchStep::Pending),
                (true, None) => Ok(SearchStep::NotFound),
                (true, Some((row, before))) => {
                    let columns = self.state.columns_of(row)?;
                    Ok(match columns.len().checked_sub(1) {
                        Some(rank) => found(row, columns[rank], before, rank, from.is_some()),
                        None => SearchStep::NotFound,
                    })
                }
            };
        }
    }

    /// The `i`th matching row and the matching cells before it.
    fn match_row(&self, i: usize) -> Option<(usize, u64)> {
        let found = self.state.lock();
        (i < found.rows.len()).then(|| (found.row(i), found.before(i)))
    }

    /// The 1-based number of the match at `place` among all of them ("k of
    /// N"), or `None` if that cell isn't a match found so far.
    ///
    /// # Errors
    ///
    /// As for [`step`](Self::step).
    pub fn ordinal(&self, place: Place) -> Result<Option<u64>, ReadError> {
        let before = {
            let found = self.state.lock();
            let i = found.rows.partition_point(|&row| to_usize(row) < place.row);
            if i >= found.rows.len() || found.row(i) != place.row {
                return Ok(None);
            }
            found.before(i)
        };
        let columns = self.state.columns_of(place.row)?;
        Ok(columns
            .iter()
            .position(|&column| column == place.column)
            .map(|rank| before + to_u64(rank) + 1))
    }

    /// The matches found so far among rows `rows` and fields `columns` (a
    /// window of the grid), with where the query is in the first
    /// `max_chars` characters of each, for the grid's highlights. Only the
    /// matching rows are read.
    ///
    /// # Errors
    ///
    /// As for [`step`](Self::step).
    pub fn matches_in(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
        max_chars: usize,
    ) -> Result<Vec<CellMatch>, ReadError> {
        let wanted: Vec<usize> = {
            let found = self.state.lock();
            let start = found
                .rows
                .partition_point(|&row| to_usize(row) < rows.start);
            let end = found.rows.partition_point(|&row| to_usize(row) < rows.end);
            found.rows[start..end.max(start)]
                .iter()
                .map(|&row| to_usize(row))
                .collect()
        };
        let state = &self.state;
        let mut matches = Vec::new();
        for row in wanted {
            let parser = &state.reading.parser;
            let Some(RowRead { bytes, base }) = state.row_bytes(row)? else {
                continue;
            };
            let Some(parsed) = parser.parse_row_in(&state.reading.index, row, &bytes, base) else {
                continue;
            };
            for (column, field) in parsed.fields().iter().enumerate() {
                if !columns.contains(&column) {
                    continue;
                }
                let value = parser.display_value_in(&bytes, base, field);
                if state.matcher.is_match(&value) {
                    matches.push(CellMatch {
                        row,
                        column,
                        ranges: state.matcher.utf16_ranges(&value, max_chars),
                    });
                }
            }
        }
        Ok(matches)
    }
}

#[cfg(test)]
impl Search {
    /// Runs `hook` in the next step, between its look at the matches and
    /// its look at whether the search is complete.
    pub(super) fn before_settling(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .state
            .before_settling
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.job.cancel();
    }
}

impl std::fmt::Debug for Search {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Search")
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

/// A found step: the match at `row`, `column`, the `rank`th matching cell
/// of its row, with `before` matching cells before the row.
fn found(row: usize, column: usize, before: u64, rank: usize, wrapped: bool) -> SearchStep {
    SearchStep::Found {
        place: Place { row, column },
        ordinal: before + to_u64(rank) + 1,
        wrapped,
    }
}

impl SearchState {
    /// The test hook (see the field); nothing outside tests.
    #[cfg_attr(not(test), expect(clippy::unused_self))]
    fn before_settling(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .before_settling
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            hook();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Found> {
        self.found.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The job: search the rows indexed so far, a chunk at a time, waiting
    /// for the index whenever the search catches up with it.
    fn run(&self, job: &Job) -> Result<SearchSummary, JobError> {
        let index = &self.reading.index;
        let mut next = self.first_row;
        loop {
            job.checkpoint()?;
            let indexed = index.row_count();
            if next >= indexed {
                match rows_settled(&self.reading, job)? {
                    Settled::Done => {
                        if next >= index.row_count() {
                            break;
                        }
                    }
                    Settled::Waiting => thread::sleep(WAIT_FOR_INDEX),
                }
                continue;
            }
            let end = self.chunk_end(next, indexed);
            let hits = self.search_rows(next..end)?;
            let mut found = self.lock();
            for (row, cells) in hits {
                let total = found.total() + u64::from(cells);
                found.rows.push(u32::try_from(row).unwrap_or(u32::MAX));
                found.ends.push(total);
            }
            found.searched = end;
            drop(found);
            next = end;
        }
        let mut found = self.lock();
        found.complete = true;
        found.searched = found.searched.max(index.row_count());
        Ok(SearchSummary {
            matches: found.total(),
            rows: found.rows.len(),
        })
    }

    /// The end of the chunk that starts at row `next`: about
    /// [`SEARCH_CHUNK_BYTES`] of rows, at least one, at most `indexed`.
    fn chunk_end(&self, next: usize, indexed: usize) -> usize {
        let index = &self.reading.index;
        let Some(start) = index.row_extent(next).map(|extent| extent.start) else {
            return next + 1;
        };
        index
            .row_at_offset(start.saturating_add(SEARCH_CHUNK_BYTES))
            .map_or(indexed, |row| row.max(next + 1))
            .min(indexed)
    }

    /// The rows of `rows` with matches, and how many cells of each match.
    fn search_rows(&self, rows: Range<usize>) -> Result<Vec<(usize, u32)>, JobError> {
        let index = &*self.reading.index;
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok(Vec::new());
        };
        let bytes = self.read(extent.clone())?;
        let base = extent.start;
        let mut hits = Vec::new();
        // `at` is where `row` starts, in `bytes`.
        let mut at = 0;
        let mut row = rows.start;
        while row < rows.end {
            let Some(candidate) = self.matcher.raw_candidates(&bytes, at) else {
                break;
            };
            let holder = if candidate == at {
                row
            } else {
                index.row_at_offset(base + candidate).unwrap_or(rows.end)
            };
            if holder >= rows.end {
                break;
            }
            let cells = self.cells_matching(holder, &bytes, base);
            if cells > 0 {
                hits.push((holder, cells));
            }
            let Some(holder_extent) = index.row_extent(holder) else {
                break;
            };
            at = holder_extent.end - base;
            row = holder + 1;
        }
        Ok(hits)
    }

    /// How many of row `row`'s cells match. `bytes` are the file's from
    /// `base` on, and hold the row.
    ///
    /// Only fields where a raw candidate starts are decoded and checked: a
    /// match in a field's display value is a candidate in the field's own
    /// bytes ([`Matcher::new`]), so a field with none can't match. (Without
    /// the raw search, every field's start is a candidate, so every
    /// non-empty field is checked.) In a row of 12 fields with the query in
    /// one, that is one check, not 12.
    fn cells_matching(&self, row: usize, bytes: &[u8], base: usize) -> u32 {
        let parser = &self.reading.parser;
        let Some(parsed) = parser.parse_row_in(&self.reading.index, row, bytes, base) else {
            return 0;
        };
        let mut count = 0;
        // Only this row's bytes, so a search for the next candidate stops
        // at its end rather than running on into rows the caller searches
        // next.
        let bytes = bytes
            .get(..parsed.span().end.saturating_sub(base))
            .unwrap_or(bytes);
        // The first candidate at or after the current field's start.
        let mut candidate = None;
        for field in parsed.fields() {
            let start = field.start().saturating_sub(base);
            let end = start + field.len();
            if candidate.is_none_or(|at| at < start) {
                candidate = self.matcher.raw_candidates(bytes, start);
            }
            let Some(at) = candidate else {
                break;
            };
            if at < end
                && self
                    .matcher
                    .is_match(&parser.display_value_in(bytes, base, field))
            {
                count += 1;
            }
        }
        count
    }

    /// Which of row `row`'s fields match, in order.
    fn columns_of(&self, row: usize) -> Result<Vec<usize>, ReadError> {
        let Some(RowRead { bytes, base }) = self.row_bytes(row)? else {
            return Ok(Vec::new());
        };
        let parser = &self.reading.parser;
        let Some(parsed) = parser.parse_row_in(&self.reading.index, row, &bytes, base) else {
            return Ok(Vec::new());
        };
        Ok(parsed
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, field)| {
                self.matcher
                    .is_match(&parser.display_value_in(&bytes, base, field))
            })
            .map(|(column, _)| column)
            .collect())
    }

    /// Row `row`'s bytes (line ending included) and their offset in the
    /// file, if the index has the row.
    fn row_bytes(&self, row: usize) -> Result<Option<RowRead<'_>>, ReadError> {
        let Some(extent) = self.reading.index.row_extent(row) else {
            return Ok(None);
        };
        let base = extent.start;
        Ok(Some(RowRead {
            bytes: self.read(extent)?,
            base,
        }))
    }

    /// The file's bytes in `extent`: from the first 64 KB kept in memory,
    /// or one read of the file (a borrow of the map).
    fn read(&self, extent: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
        match self.head.get(extent.clone()) {
            Some(bytes) => Ok(Cow::Borrowed(bytes)),
            None => self.source.read_range(extent),
        }
    }
}

/// One row's bytes, line ending included, and where they start in the file.
struct RowRead<'a> {
    bytes: Cow<'a, [u8]>,
    base: usize,
}

/// Whether more rows can still come, for a job that has caught up with the
/// index ([`rows_settled`]).
pub(super) enum Settled {
    /// The index is complete: the rows there are, are all there are.
    Done,
    /// The index is still going: wait a moment and look again.
    Waiting,
}

/// For a job that has read every row indexed so far: whether the index is
/// complete, or still going. If the index job stopped first (cancelled
/// with the document, or a removable drive that vanished), so does the
/// caller, with the index's error.
pub(super) fn rows_settled(reading: &Reading, job: &Job) -> Result<Settled, JobError> {
    if reading.index.status() == Status::Complete {
        return Ok(Settled::Done);
    }
    if let Some(outcome) = reading.index_job.control().outcome() {
        // The index may have completed between the two looks.
        if reading.index.status() == Status::Complete {
            return Ok(Settled::Done);
        }
        return Err(outcome.err().unwrap_or(JobError::Cancelled));
    }
    if job.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    Ok(Settled::Waiting)
}

fn to_usize(row: u32) -> usize {
    usize::try_from(row).unwrap_or(usize::MAX)
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
