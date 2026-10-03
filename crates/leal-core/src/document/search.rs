//! **Find** over a whole document (task 1.8): a background search that
//! streams the cells it finds, and the answers the find bar needs from it.
//!
//! [`Document::find`] starts a [`Search`] as a P2 job (DESIGN §3.10): it
//! runs on the background pool, works in chunks of about 64 KiB of rows
//! (well under rule 3's 5 ms), pauses at each chunk while the user scrolls
//! or types, and stops within a chunk when cancelled. It searches the rows
//! indexed so far and keeps up as more arrive, so it works while indexing
//! (rule 6's promise for filters, kept for find too), and ends when it has
//! searched the last row. Caught up with the index, it doesn't hold its
//! pool thread: its turn ends, and the index wakes it when it publishes
//! more rows ([`Scheduler::spawn_resumable`], p1-review conc-1). It reads
//! the rows the grid does ([`Reading::rows_from`]): after a removable
//! drive vanishes, the first 64 KB's rows too, and once the file changed
//! while it was read, only the checked copy's (p1-review fid-1, fid-2).
//!
//! [`Scheduler::spawn_resumable`]: crate::schedule::Scheduler::spawn_resumable
//!
//! **A match is a cell** whose display value holds the query
//! ([`Matcher`]). The count and "k of N" count cells, in the document's
//! order: row by row, and left to right within a row. The header row (the
//! logical row 0, if the file has one) isn't one of the grid's rows, so
//! its matches are left out of every answer.
//!
//! **Rows inserted and deleted** (task 2.4a, `docs/tasks/2.4.md` §5). The
//! file's rows keep their physical row as their key, which is their
//! logical order too, so nothing is remapped when rows come and go: a
//! chunk passes over deleted rows, and a delete (or its undo) is caught up
//! like a cell edit, by the rows' ids, a deleted row counting 0. Inserted
//! rows are in memory: they are all searched when the search starts, kept
//! in logical order with running counts of their own, and caught up the
//! same way. A query adds both counts, with logical places worked out in
//! the piece list the counts are right for.
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
//! every value is checked. An edited row is never passed over: if the
//! query is nowhere in its bytes, only its edited values are checked,
//! otherwise every cell as it reads now.
//!
//! **Edits** (ADR-0008 decision 2). A match is a cell as it reads now, so
//! an edited cell matches on its new value, and stops matching on the one
//! it replaced. A search keeps up with edits made while it runs or after it
//! finished: the document logs the row each edit touches, and the search
//! recounts the rows logged since it last caught up, among those it has
//! searched, then rebuilds its running counts in one pass (O(M + K) for M
//! matching rows and K edited ones). The search's job catches up after
//! each chunk; a chunk searched with older edits than the latest is caught
//! up the same way, so no count is ever of a mixture. A query from the main
//! thread does at most a little of it itself (a few rows, while the counts
//! are small), and otherwise starts a job to do it and says the counts are
//! catching up ([`SearchProgress::catching_up`]).

use std::borrow::Cow;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::task::Poll;

use super::{Document, Place, Reading, RowView, bytes_in};
use crate::edit::{
    Columns, InsertedRow, Overlay, OverlayRow, RowEdits, RowId, RowMap, Segment, Slot,
};
use crate::find::{FindError, Matcher, Query};
use crate::index::{RowIndex, Status};
use crate::rows::RowParser;
use crate::schedule::{Interval, Job, JobControl, JobError, JobHandle, Priority, Scheduler};
use crate::source::{ReadError, Source};

/// The most edited rows a main-thread query recounts itself before it
/// leaves the rest to a job.
const INLINE_ROWS: usize = 64;

/// The most matching rows a main-thread query rebuilds the running counts
/// over (a pass over them, about a tenth of a millisecond at this size).
const INLINE_MATCHING_ROWS: usize = 1 << 16;

/// How many rows a catch-up job recounts between checkpoints (DESIGN §3.10
/// rule 3): a millisecond or so of rows of ordinary width.
const CATCH_UP_ROWS: usize = 256;

/// Who is catching a search's counts up ([`SearchState::catch_up`]).
#[derive(Clone, Copy)]
enum CatchUp<'j> {
    /// A query on the caller's thread, perhaps the main thread: a little
    /// work at most, never waiting.
    Inline,
    /// A job, which checkpoints as it goes.
    Job(&'j Job),
}

/// About how many bytes of rows one chunk of a search covers.
pub const SEARCH_CHUNK_BYTES: usize = 64 << 10;

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
    /// Rows of the file searched so far: every physical row before this
    /// one.
    pub rows_searched: usize,
    /// Whether every row has been searched.
    pub complete: bool,
    /// Whether edits made since are still being counted, in the background:
    /// the matches and "k of N" may be behind until it is false again.
    pub catching_up: bool,
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
    /// The logical row.
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
/// whose display value holds the query ([`Matcher`]); the header row's
/// matches are left out.
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
    /// The matches, and where the search has got to.
    found: Mutex<Found>,
    /// For catch-up jobs.
    scheduler: Scheduler,
    /// Held while the counts catch up with edits, so one catch-up runs at a
    /// time.
    catching_up: Mutex<()>,
    /// A catch-up job has been started and hasn't finished.
    catch_up_started: AtomicBool,
    /// The latest catch-up job, to cancel when the search is dropped, and
    /// for the app to wait on.
    catch_up_job: Mutex<Option<JobHandle<()>>>,
    /// The search was dropped: no more catch-up jobs.
    dropped: AtomicBool,
    /// The search's job (or a catch-up job, once it has finished) is
    /// searching the file's rows: after the search starts again because of
    /// a column insert or delete, a catch-up job searches them if the
    /// search's own job has finished.
    turning: AtomicBool,
    /// Run once by the next step, between its look at the matches and its
    /// look at whether the search is complete: a test makes the search
    /// finish there.
    #[cfg(test)]
    before_settling: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once by the job between searching a chunk and adding it: a test
    /// edits there.
    #[cfg(test)]
    in_chunk: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// An inserted row as it is now, with its edits: `None` if it is gone.
type InsertedNow = Option<(Arc<InsertedRow>, Option<Arc<RowEdits>>)>;

/// An inserted row with matches (task 2.4a).
#[derive(Clone, Copy, Debug)]
struct InsertedMatch {
    /// The inserted row's number, and its gap.
    n: u32,
    gap: u32,
    /// Its logical row, in [`Found::map`].
    logical: usize,
    /// Its matching cells.
    cells: u64,
}

/// A matching row, for a step: its logical row, which row it is, and the
/// matching cells before it.
#[derive(Clone, Copy, Debug)]
struct MatchRow {
    logical: usize,
    slot: Slot,
    before: u64,
}

/// What [`Found::next_from`] finds.
enum Next {
    Row(MatchRow),
    /// An inserted row matches, but the file's rows before it haven't all
    /// been searched yet.
    NotYet,
    None,
}

/// The matches found so far.
#[derive(Debug, Default)]
struct Found {
    /// Rows of the file with a match, by physical row, in order (which is
    /// their logical order too: original rows never reorder). Deleted rows
    /// aren't in it; the header row may be ([`Found::header_cells`]).
    rows: Vec<u32>,
    /// `ends[i]`: matching cells in `rows[..=i]`.
    ends: Vec<u64>,
    /// Inserted rows with a match, in logical order (task 2.4a). They are
    /// in memory, so they are all searched when the search starts.
    inserted: Vec<InsertedMatch>,
    /// `inserted_ends[i]`: matching cells in `inserted[..=i]`.
    inserted_ends: Vec<u64>,
    /// The piece list the counts are right for (as of `synced`): logical
    /// places are worked out with it.
    map: RowMap,
    /// The file has a header row.
    header: bool,
    /// Every physical row before this one has been searched.
    searched: usize,
    complete: bool,
    /// Some of the rows searched were read from the first 64 KB kept in
    /// memory, so the search starts again if those turn out stale.
    used_head: bool,
    /// The counts are right for the edits up to this version of the
    /// document's edits (`EditStore::since`).
    synced: usize,
    /// The column inserts and deletes the counts are right for, by their
    /// generation: when it changes, the search starts again (ADR-0014
    /// decision 2).
    generation: u64,
    /// How many times the search has started again: a chunk searched
    /// before the latest start is dropped.
    epoch: u64,
    /// It started again and hasn't finished since.
    restarted: bool,
}

impl Found {
    /// Every match: in the file's rows and the inserted ones, less the
    /// header row's.
    fn total(&self) -> u64 {
        let originals = self.ends.last().copied().unwrap_or(0);
        let inserted = self.inserted_ends.last().copied().unwrap_or(0);
        (originals + inserted).saturating_sub(self.header_cells())
    }

    /// The matching cells of the header row (logical row 0) if the file
    /// has one: it is searched like any row, and left out of the answers.
    fn header_cells(&self) -> u64 {
        if !self.header {
            return 0;
        }
        match self.map.slot(0) {
            Some(Slot::Original(row)) => match self.rows.binary_search(&row) {
                Ok(i) => self.ends[i] - self.before(i),
                Err(_) => 0,
            },
            Some(Slot::Inserted(n)) => self
                .inserted
                .first()
                .filter(|m| m.n == n)
                .map_or(0, |m| m.cells),
            None => 0,
        }
    }

    /// Each of `counts`' rows (searched rows, in order, each once) has
    /// that many matching cells now: the rows and their running counts are
    /// rebuilt in one pass, O(M + K) for M matching rows and K counts.
    fn merge(&mut self, counts: &[(usize, u64)]) {
        if counts.is_empty() {
            return;
        }
        let mut rows = Vec::with_capacity(self.rows.len() + counts.len());
        let mut ends = Vec::with_capacity(rows.capacity());
        let mut total = 0;
        let mut push = |row: usize, cells: u64| {
            if cells > 0 {
                total += cells;
                rows.push(u32::try_from(row).unwrap_or(u32::MAX));
                ends.push(total);
            }
        };
        let mut new = counts.iter().peekable();
        for i in 0..self.rows.len() {
            let row = self.row(i);
            while let Some(&&(edited, cells)) = new.peek()
                && edited < row
            {
                push(edited, cells);
                new.next();
            }
            match new.peek() {
                Some(&&(edited, cells)) if edited == row => {
                    push(edited, cells);
                    new.next();
                }
                _ => push(row, self.ends[i] - self.before(i)),
            }
        }
        for &(edited, cells) in new {
            push(edited, cells);
        }
        self.rows = rows;
        self.ends = ends;
    }

    /// The inserted rows `counts` (each inserted row's number, gap and
    /// matching cells now; 0 if it is gone) are recounted, and every
    /// inserted row's logical row is worked out again in `map`, which the
    /// counts become right for. O(I log P) for I matching inserted rows.
    fn merge_inserted(&mut self, counts: &[(u32, u32, u64)], map: RowMap) {
        if !counts.is_empty() {
            let touched: std::collections::HashSet<u32> =
                counts.iter().map(|&(n, _, _)| n).collect();
            self.inserted.retain(|m| !touched.contains(&m.n));
            self.inserted
                .extend(counts.iter().filter(|&&(_, _, cells)| cells > 0).map(
                    |&(n, gap, cells)| InsertedMatch {
                        n,
                        gap,
                        logical: 0,
                        cells,
                    },
                ));
        }
        self.map = map;
        if self.inserted.is_empty() && self.inserted_ends.is_empty() {
            return;
        }
        for m in &mut self.inserted {
            m.logical = self
                .map
                .logical_of_inserted(m.n, m.gap)
                .unwrap_or(usize::MAX);
        }
        self.inserted.retain(|m| m.logical != usize::MAX);
        self.inserted.sort_unstable_by_key(|m| m.logical);
        let mut total = 0;
        self.inserted_ends = self
            .inserted
            .iter()
            .map(|m| {
                total += m.cells;
                total
            })
            .collect();
    }

    /// Matching cells before `rows[i]`.
    fn before(&self, i: usize) -> u64 {
        i.checked_sub(1).map_or(0, |prev| self.ends[prev])
    }

    fn row(&self, i: usize) -> usize {
        to_usize(self.rows[i])
    }

    /// The matching cells before logical row `logical`, the header row's
    /// left out.
    fn before_logical(&self, logical: usize) -> u64 {
        let physical = self.map.physical_at_or_after(logical);
        let i = self.rows.partition_point(|&row| to_usize(row) < physical);
        let j = self.inserted.partition_point(|m| m.logical < logical);
        let inserted = j.checked_sub(1).map_or(0, |j| self.inserted_ends[j]);
        let header = if logical > 0 { self.header_cells() } else { 0 };
        (self.before(i) + inserted).saturating_sub(header)
    }

    /// The first logical row searched: 1 if the file has a header row.
    fn first(&self) -> usize {
        usize::from(self.header)
    }

    /// The first matching row at or after logical row `start`, among those
    /// found so far.
    fn next_from(&self, start: usize) -> Next {
        let start = start.max(self.first());
        let physical = self.map.physical_at_or_after(start);
        let mut i = self.rows.partition_point(|&row| to_usize(row) < physical);
        let j = self.inserted.partition_point(|m| m.logical < start);
        // The file's next matching row still in the document.
        let original = loop {
            let Some(&row) = self.rows.get(i) else {
                break None;
            };
            match self.map.logical_of(row) {
                Ok(logical) => break Some((logical, row)),
                Err(_) => i += 1,
            }
        };
        let inserted = self
            .inserted
            .get(j)
            .filter(|m| original.is_none_or(|(logical, _)| m.logical < logical));
        let row = match (original, inserted) {
            (_, Some(m)) => {
                // Every row of the file before it must have been searched.
                if !self.complete && self.map.physical_at_or_after(m.logical) > self.searched {
                    return Next::NotYet;
                }
                MatchRow {
                    logical: m.logical,
                    slot: Slot::Inserted(m.n),
                    before: 0,
                }
            }
            (Some((logical, row)), None) => MatchRow {
                logical,
                slot: Slot::Original(row),
                before: 0,
            },
            (None, None) => return Next::None,
        };
        Next::Row(MatchRow {
            before: self.before_logical(row.logical),
            ..row
        })
    }

    /// The last matching row before logical row `end`, among those found
    /// so far.
    fn previous_before(&self, end: usize) -> Option<MatchRow> {
        let physical = self.map.physical_at_or_after(end);
        let mut i = self.rows.partition_point(|&row| to_usize(row) < physical);
        let j = self.inserted.partition_point(|m| m.logical < end);
        let original = loop {
            let Some(k) = i.checked_sub(1) else {
                break None;
            };
            let row = self.rows[k];
            match self.map.logical_of(row) {
                Ok(logical) => break Some((logical, row)),
                Err(_) => i = k,
            }
        };
        let inserted = j
            .checked_sub(1)
            .map(|j| self.inserted[j])
            .filter(|m| original.is_none_or(|(logical, _)| m.logical > logical));
        let row = match (original, inserted) {
            (_, Some(m)) => MatchRow {
                logical: m.logical,
                slot: Slot::Inserted(m.n),
                before: 0,
            },
            (Some((logical, row)), None) => MatchRow {
                logical,
                slot: Slot::Original(row),
                before: 0,
            },
            (None, None) => return None,
        };
        (row.logical >= self.first()).then(|| MatchRow {
            before: self.before_logical(row.logical),
            ..row
        })
    }

    /// Whether every row up to and including logical row `logical` has
    /// been searched.
    fn covers(&self, logical: usize) -> bool {
        match self.map.slot(logical) {
            Some(Slot::Original(row)) => to_usize(row) < self.searched,
            _ => self.map.physical_at_or_after(logical) <= self.searched,
        }
    }

    /// The matching rows among logical rows `rows`, in order.
    fn rows_in(&self, rows: Range<usize>) -> Vec<(usize, Slot)> {
        let rows = rows.start.max(self.first())..rows.end;
        if rows.start >= rows.end {
            return Vec::new();
        }
        let from = self.map.physical_at_or_after(rows.start);
        let to = self.map.physical_at_or_after(rows.end);
        let start = self.rows.partition_point(|&row| to_usize(row) < from);
        let end = self.rows.partition_point(|&row| to_usize(row) < to);
        let mut wanted: Vec<(usize, Slot)> = self.rows[start..end.max(start)]
            .iter()
            .filter_map(|&row| Some((self.map.logical_of(row).ok()?, Slot::Original(row))))
            .collect();
        let first = self.inserted.partition_point(|m| m.logical < rows.start);
        let last = self.inserted.partition_point(|m| m.logical < rows.end);
        wanted.extend(
            self.inserted[first..last.max(first)]
                .iter()
                .map(|m| (m.logical, Slot::Inserted(m.n))),
        );
        wanted.sort_unstable_by_key(|&(logical, _)| logical);
        wanted
    }
}

impl Document {
    /// Starts searching the current reading for `query` (the find bar,
    /// task 1.8), as a P2 job (see [`Search`]). It returns at once; the
    /// [`Search`] gives what it has found so far. Inserted rows (task 2.4a)
    /// are in memory and are searched here, before it returns.
    ///
    /// # Errors
    ///
    /// The [`FindError`] for an empty or too long query.
    pub fn find(&self, query: &Query) -> Result<Search, FindError> {
        let reading = self.current();
        let matcher = Matcher::new(query, reading.detection.encoding)?;
        let header = reading.detection.header;
        // A search starts from the edits as they are when it starts.
        let (overlay, synced, generation) = reading.edits.snapshot_columns();
        let mut found = Found {
            header,
            synced,
            generation,
            ..Found::default()
        };
        let counts = inserted_counts(&reading.parser, &matcher, &overlay);
        found.merge_inserted(&counts, overlay.map().clone());
        drop(overlay);
        let state = Arc::new(SearchState {
            source: Arc::clone(&reading.source),
            head: Arc::clone(&reading.head),
            reading,
            matcher,
            found: Mutex::new(found),
            scheduler: self.scheduler.clone(),
            catching_up: Mutex::new(()),
            catch_up_started: AtomicBool::new(false),
            catch_up_job: Mutex::default(),
            dropped: AtomicBool::new(false),
            turning: AtomicBool::new(true),
            #[cfg(test)]
            before_settling: Mutex::default(),
            #[cfg(test)]
            in_chunk: Mutex::default(),
        });
        let worker = Arc::clone(&state);
        let job = self
            .scheduler
            .spawn_resumable(Priority::P2, Interval::Find, move |job| worker.turn(job));
        Ok(Search { state, job })
    }
}

/// Every inserted row in `overlay` with a match: its number, gap and
/// matching cells.
fn inserted_counts(
    parser: &RowParser,
    matcher: &Matcher,
    overlay: &Overlay,
) -> Vec<(u32, u32, u64)> {
    let map = overlay.map();
    let Some(len) = map.len() else {
        return Vec::new();
    };
    let mut counts = Vec::new();
    for segment in map.segments(0..len) {
        let Segment::Inserted(range) = segment else {
            continue;
        };
        for n in range {
            let Some(row) = overlay.inserted(n) else {
                continue;
            };
            let cells =
                inserted_cells_matching(parser, matcher, row, overlay.cells_of(RowId::inserted(n)));
            if cells > 0 {
                counts.push((n, row.gap(), cells));
            }
        }
    }
    counts
}

/// How many of a row's cells match, as it reads.
fn count_matching(view: &RowView<'_>, matcher: &Matcher) -> u32 {
    let matching = view
        .filled()
        .into_iter()
        .filter(|&(_, cell)| matcher.is_match(&view.value_of(cell)))
        .count();
    u32::try_from(matching).unwrap_or(u32::MAX)
}

/// How many of an inserted row's cells match, as it reads with `cells`.
fn inserted_cells_matching(
    parser: &RowParser,
    matcher: &Matcher,
    row: &InsertedRow,
    cells: OverlayRow<'_>,
) -> u64 {
    let view = RowView::inserted(parser, row, cells);
    u64::from(count_matching(&view, matcher))
}

impl Search {
    /// The search's job: to wait for it, or cancel it.
    #[must_use]
    pub fn job(&self) -> &JobHandle<SearchSummary> {
        &self.job
    }

    /// Stops the search within one chunk, and any catch-up job. What it
    /// found is kept.
    pub fn cancel(&self) {
        self.job.cancel();
        self.state.cancel_catch_up();
    }

    /// The reading searched.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.state.reading.generation
    }

    /// Where the search has got to. Edits made since it last caught up
    /// are counted first if that is quick; otherwise a job counts them,
    /// and `catching_up` says so.
    #[must_use]
    pub fn progress(&self) -> SearchProgress {
        // A row that can't be read now is recounted next time.
        let caught_up = self.catch_up().unwrap_or(false);
        let found = self.state.lock();
        SearchProgress {
            generation: self.state.reading.generation,
            matches: found.total(),
            rows_searched: found.searched,
            complete: found.complete,
            catching_up: !caught_up,
        }
    }

    /// Catches the counts up with the edits, if that is quick, and
    /// otherwise starts a job to: whether they are caught up now.
    fn catch_up(&self) -> Result<bool, ReadError> {
        if self.state.catch_up(CatchUp::Inline)? {
            return Ok(true);
        }
        SearchState::start_catch_up(&self.state);
        Ok(false)
    }

    /// The job catching the counts up with edits, while one is running
    /// ([`SearchProgress::catching_up`]): wait on it rather than poll, then
    /// look at the progress again (more edits may have come meanwhile).
    #[must_use]
    pub fn catch_up_job(&self) -> Option<JobHandle<()>> {
        self.state
            .catch_up_job
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|job| !job.control().is_finished())
            .cloned()
    }

    /// **Next** (`forward`) or **Previous** from the cell `from` (a
    /// logical row and a field), which need not be a match: the first
    /// match after it, or the last one before it, in the document's order.
    /// With no `from`, the first match (forward) or the last one. Past the
    /// last match it wraps round to the first, and before the first to the
    /// last, once the search is complete; until then it is
    /// [`SearchStep::Pending`]. It is also `Pending` while edits are being
    /// counted ([`SearchProgress::catching_up`]).
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if a matching row can't be read again (a removable
    /// drive that vanished).
    pub fn step(&self, from: Option<Place>, forward: bool) -> Result<SearchStep, ReadError> {
        // The step needs every edit counted: until then, ask again.
        if !self.catch_up()? {
            return Ok(SearchStep::Pending);
        }
        if forward {
            self.step_forward(from)
        } else {
            self.step_backward(from)
        }
    }

    fn step_forward(&self, from: Option<Place>) -> Result<SearchStep, ReadError> {
        let mut start = from.map_or(0, |place| place.row);
        loop {
            // The first matching row at or after `from`'s, among the
            // matches found by now.
            loop {
                // The lock is let go before the row is read.
                let next = self.state.lock().next_from(start);
                let Next::Row(row) = next else {
                    break;
                };
                let columns = self.state.columns_of(row.slot)?;
                let next = match from {
                    Some(place) if place.row == row.logical => {
                        columns.iter().position(|&column| column > place.column)
                    }
                    _ => (!columns.is_empty()).then_some(0),
                };
                if let Some(rank) = next {
                    return Ok(found(row.logical, columns[rank], row.before, rank, false));
                }
                start = row.logical + 1;
            }
            self.state.before_settling();
            let (complete, first) = {
                let found = self.state.lock();
                // A chunk may have landed since the last look, and the
                // search may have finished with it: its rows come first.
                if matches!(found.next_from(start), Next::Row(_)) {
                    continue;
                }
                let first = match found.next_from(0) {
                    Next::Row(row) => Some(row),
                    Next::NotYet | Next::None => None,
                };
                (found.complete, first)
            };
            return match (complete, first) {
                (false, _) => Ok(SearchStep::Pending),
                (true, None) => Ok(SearchStep::NotFound),
                (true, Some(row)) => {
                    let columns = self.state.columns_of(row.slot)?;
                    Ok(match columns.first() {
                        Some(&column) => found(row.logical, column, 0, 0, true),
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
            |found: &Found| found.complete || from.is_some_and(|place| found.covers(place.row));
        let mut end = from.map_or(usize::MAX, |place| place.row.saturating_add(1));
        loop {
            let (mut row, was_settled) = {
                let found = self.state.lock();
                (found.previous_before(end), settled(&found))
            };
            if was_settled {
                while let Some(matching) = row {
                    let columns = self.state.columns_of(matching.slot)?;
                    let previous = match from {
                        Some(place) if place.row == matching.logical => {
                            columns.iter().rposition(|&column| column < place.column)
                        }
                        _ => columns.len().checked_sub(1),
                    };
                    if let Some(rank) = previous {
                        return Ok(found(
                            matching.logical,
                            columns[rank],
                            matching.before,
                            rank,
                            false,
                        ));
                    }
                    end = matching.logical;
                    row = self.state.lock().previous_before(end);
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
                (found.complete, found.previous_before(usize::MAX))
            };
            return match (complete, last) {
                (false, _) => Ok(SearchStep::Pending),
                (true, None) => Ok(SearchStep::NotFound),
                (true, Some(row)) => {
                    let columns = self.state.columns_of(row.slot)?;
                    Ok(match columns.len().checked_sub(1) {
                        Some(rank) => {
                            found(row.logical, columns[rank], row.before, rank, from.is_some())
                        }
                        None => SearchStep::NotFound,
                    })
                }
            };
        }
    }

    /// The 1-based number of the match at `place` among all of them ("k of
    /// N"), or `None` if that cell isn't a match found so far.
    ///
    /// # Errors
    ///
    /// As for [`step`](Self::step).
    pub fn ordinal(&self, place: Place) -> Result<Option<u64>, ReadError> {
        self.catch_up()?;
        let (slot, before) = {
            let found = self.state.lock();
            let Some((logical, slot)) = found
                .rows_in(place.row..place.row.saturating_add(1))
                .first()
                .copied()
            else {
                return Ok(None);
            };
            (slot, found.before_logical(logical))
        };
        let columns = self.state.columns_of(slot)?;
        Ok(columns
            .iter()
            .position(|&column| column == place.column)
            .map(|rank| before + to_u64(rank) + 1))
    }

    /// The matches found so far among logical rows `rows` and fields
    /// `columns` (a window of the grid), with where the query is in the
    /// first `max_chars` characters of each, for the grid's highlights.
    /// Only the matching rows are read.
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
        self.catch_up()?;
        let wanted = self.state.lock().rows_in(rows);
        let state = &self.state;
        let mut matches = Vec::new();
        for (row, slot) in wanted {
            state.with_view(slot, |view| {
                for column in columns.start..columns.end.min(view.len()) {
                    let Some(value) = view.value(column) else {
                        continue;
                    };
                    if state.matcher.is_match(&value) {
                        matches.push(CellMatch {
                            row,
                            column,
                            ranges: state.matcher.utf16_ranges(&value, max_chars),
                        });
                    }
                }
            })?;
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

    /// Runs `hook` in the job, once, between searching its next chunk and
    /// adding it.
    pub(super) fn in_chunk(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .state
            .in_chunk
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(hook));
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.job.cancel();
        // A catch-up job holds the reading: it mustn't outlive the search.
        self.state.dropped.store(true, Ordering::Release);
        self.state.cancel_catch_up();
    }
}

impl std::fmt::Debug for Search {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The fields as they are: `progress` would catch up first.
        let found = self.state.lock();
        f.debug_struct("Search")
            .field("generation", &self.state.reading.generation)
            .field("matches", &found.total())
            .field("rows_searched", &found.searched)
            .field("complete", &found.complete)
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

    /// The rows searches read ([`Reading::rows_from`] the reading's
    /// index), with the first 64 KB trusted unless `head_stale`.
    fn rows_index(&self, head_stale: bool) -> (&RowIndex, usize) {
        self.reading.rows_from(&self.reading.index, head_stale)
    }

    /// One turn of the job: search the rows that can be read, a chunk at a
    /// time, and once caught up with the index, end the turn until it has
    /// more ([`more_rows`]). The search's place is `found.searched`, so a
    /// turn starts where the last one stopped. The file's rows are searched
    /// by physical row, passing over deleted ones (task 2.4a); inserted
    /// rows were searched when the search started.
    ///
    /// Each step looks once at whether the first 64 KB are stale, for both
    /// the rows and their bytes. If they turn stale after the search read
    /// rows from them, it starts again from the first row, over the checked
    /// copy's rows only: a search never finishes with matches from both
    /// versions of the file (p1-review fid-2).
    fn turn(&self, job: &Job) -> Poll<Result<SearchSummary, JobError>> {
        let polled = self.search_on(job);
        if polled.is_ready() {
            self.turning.store(false, Ordering::Release);
        }
        polled
    }

    /// [`turn`](Self::turn)'s search, until the rows that can be read have
    /// been.
    fn search_on(&self, job: &Job) -> Poll<Result<SearchSummary, JobError>> {
        loop {
            loop {
                job.checkpoint()?;
                self.restart_if_columns_changed();
                let stale = self.source.changed_on_disk();
                let (next, epoch) = {
                    let mut found = self.lock();
                    if stale && found.used_head {
                        found.rows.clear();
                        found.ends.clear();
                        found.searched = 0;
                        found.used_head = false;
                    }
                    (found.searched, found.epoch)
                };
                let indexed = self.reading.index.row_count();
                let (index, available) = self.rows_index(stale);
                if next >= available {
                    let control = self.reading.index_job.control();
                    match more_rows(&self.reading.index, control, indexed, job)? {
                        Settled::Done => {
                            // No more rows will come. Finished, unless the
                            // index or the file moved on since this step
                            // looked.
                            let now = self.source.changed_on_disk();
                            if now == stale && next >= self.rows_index(now).1 {
                                break;
                            }
                            continue;
                        }
                        Settled::Waiting => return Poll::Pending,
                    }
                }
                let end = chunk_end(index, next, available);
                // The chunk is searched with its rows' edits, which of them
                // are still in the document and the column operations, as
                // they are now, without holding the rest of the overlay (so
                // an edit meanwhile doesn't copy it); any made meanwhile
                // are caught up as it is added.
                let chunk = self.reading.edits.rows_in(next..end);
                let (hits, from_head) = self.search_rows(
                    index,
                    next..end,
                    stale,
                    &chunk.edited,
                    &chunk.live,
                    &chunk.columns,
                )?;
                let version = chunk.version;
                let generation = chunk.column_generation;
                drop(chunk);
                self.in_chunk();
                let mut found = self.lock();
                if found.epoch != epoch || found.generation != generation {
                    // Started again meanwhile, or about to: not these rows.
                    continue;
                }
                for (row, cells) in hits {
                    let total = found.ends.last().copied().unwrap_or(0) + u64::from(cells);
                    found.rows.push(u32::try_from(row).unwrap_or(u32::MAX));
                    found.ends.push(total);
                }
                found.searched = end;
                found.used_head |= from_head;
                // The chunk's rows are right as of `version`: anything
                // edited since is recounted.
                found.synced = found.synced.min(version);
                drop(found);
                self.catch_up(CatchUp::Job(job))?;
            }
            let epoch = self.lock().epoch;
            if !self.catch_up(CatchUp::Job(job))? {
                // Cancelled part-way.
                return Poll::Ready(Err(JobError::Cancelled));
            }
            let available = self.rows_index(self.source.changed_on_disk()).1;
            let mut found = self.lock();
            if found.epoch != epoch {
                // A column insert or delete started it again.
                continue;
            }
            found.complete = true;
            found.restarted = false;
            found.searched = found.searched.max(available);
            // The header row is searched like any row, but left out of the
            // answers, so it isn't counted.
            let header_row = usize::from(found.header_cells() > 0);
            return Poll::Ready(Ok(SearchSummary {
                matches: found.total(),
                rows: (found.rows.len() + found.inserted.len()).saturating_sub(header_row),
            }));
        }
    }

    /// Starts the search again if a column has been inserted or deleted
    /// since its counts were made (ADR-0014 decision 2): every row's
    /// matches may have changed. The inserted rows are counted at once, as
    /// when it starts; the file's rows are searched again by the search's
    /// job, or a catch-up job once that has finished.
    fn restart_if_columns_changed(&self) {
        let (generation, epoch) = {
            let found = self.lock();
            (found.generation, found.epoch)
        };
        if self.reading.edits.column_generation() == generation {
            return;
        }
        let (overlay, synced, generation) = self.reading.edits.snapshot_columns();
        let counts = inserted_counts(&self.reading.parser, &self.matcher, &overlay);
        let mut found = self.lock();
        if found.epoch != epoch {
            return; // another restart got there first
        }
        found.rows.clear();
        found.ends.clear();
        found.inserted.clear();
        found.inserted_ends.clear();
        found.searched = 0;
        found.complete = false;
        found.used_head = false;
        found.synced = synced;
        found.generation = generation;
        found.epoch += 1;
        found.restarted = true;
        found.merge_inserted(&counts, overlay.map().clone());
    }

    /// The rows of `rows` (in `index`) with matches, how many cells of
    /// each match, and whether they were read from the first 64 KB kept in
    /// memory (trusted unless `head_stale`), with `edits` on top. Only the
    /// rows in `live` (not deleted) are counted.
    fn search_rows(
        &self,
        index: &RowIndex,
        rows: Range<usize>,
        head_stale: bool,
        edits: &[(usize, Arc<RowEdits>)],
        live: &[Range<usize>],
        columns: &Columns,
    ) -> Result<(Vec<(usize, u32)>, bool), JobError> {
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok((Vec::new(), false));
        };
        let (bytes, from_head) = bytes_in(&self.source, &self.head, extent.clone(), head_stale)?;
        let base = extent.start;
        // Edited rows are checked as they read now, every one: the raw
        // search below can't see their edited values, so it passes over
        // them. (`edits` are in row order.)
        let edited = |row: usize| edits.binary_search_by_key(&row, |&(r, _)| r).is_ok();
        let all_live = live.len() == 1 && live[0] == rows;
        let is_live = |row: usize| {
            all_live || {
                let i = live.partition_point(|range| range.end <= row);
                live.get(i).is_some_and(|range| range.contains(&row))
            }
        };
        let mut hits = Vec::new();
        // Once a column insert's value matches (task 2.4b), a row can match
        // with nothing in its own bytes: every row is read.
        let every_row = columns
            .ops()
            .iter()
            .any(|op| op.values().any(|value| self.matcher.is_match(value)));
        if every_row {
            for row in rows.clone() {
                if !edited(row) && is_live(row) {
                    let cells = self.unedited_matching(index, row, &bytes, base, columns);
                    if cells > 0 {
                        hits.push((row, cells));
                    }
                }
            }
        }
        // `at` is where `row` starts, in `bytes`.
        let mut at = 0;
        let mut row = if every_row { rows.end } else { rows.start };
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
            if !edited(holder) && is_live(holder) {
                let cells = self.unedited_matching(index, holder, &bytes, base, columns);
                if cells > 0 {
                    hits.push((holder, cells));
                }
            }
            let Some(holder_extent) = index.row_extent(holder) else {
                break;
            };
            at = holder_extent.end - base;
            row = holder + 1;
        }
        if !edits.is_empty() {
            for (row, row_edits) in edits {
                let cells =
                    self.edited_cells_matching(index, *row, &bytes, base, Some(row_edits), columns);
                if cells > 0 {
                    hits.push((*row, cells));
                }
            }
            hits.sort_unstable_by_key(|&(row, _)| row);
        }
        Ok((hits, from_head))
    }

    /// How many of row `row`'s cells match as it reads with `edits` (none:
    /// as the file has it). `bytes` are the file's from `base` on, and hold
    /// the row. If the query is nowhere in the row's own bytes, only its
    /// edited values can match, so only they are checked.
    fn edited_cells_matching(
        &self,
        index: &RowIndex,
        row: usize,
        bytes: &[u8],
        base: usize,
        edits: Option<&RowEdits>,
        columns: &Columns,
    ) -> u32 {
        let Some(edits) = edits else {
            return self.unedited_matching(index, row, bytes, base, columns);
        };
        let parser = &self.reading.parser;
        let Some(parsed) = parser.parse_row_in(index, row, bytes, base) else {
            return 0;
        };
        let id = RowId::original(u32::try_from(row).unwrap_or(u32::MAX));
        let cells = OverlayRow {
            id,
            edits: Some(edits),
            columns,
        };
        if !columns.is_empty() {
            // Laid out by the column operations: every cell as it reads.
            let view = RowView::new(parser, bytes, base, &parsed, cells);
            return count_matching(&view, &self.matcher);
        }
        let span = parsed.span();
        let own = bytes
            .get(..span.end.saturating_sub(base))
            .unwrap_or_default();
        let anywhere = self
            .matcher
            .raw_candidates(own, span.start.saturating_sub(base))
            .is_some();
        if anywhere {
            let view = RowView::new(parser, bytes, base, &parsed, cells);
            return count_matching(&view, &self.matcher);
        }
        let matching = edits
            .shown(columns, None)
            .into_iter()
            .filter(|(_, value)| self.matcher.is_match(value))
            .count();
        u32::try_from(matching).unwrap_or(u32::MAX)
    }

    /// How many of unedited row `row`'s cells match, laid out by `columns`
    /// ([`cells_matching`](Self::cells_matching) with none).
    fn unedited_matching(
        &self,
        index: &RowIndex,
        row: usize,
        bytes: &[u8],
        base: usize,
        columns: &Columns,
    ) -> u32 {
        if columns.is_empty() {
            return self.cells_matching(index, row, bytes, base);
        }
        let parser = &self.reading.parser;
        let Some(parsed) = parser.parse_row_in(index, row, bytes, base) else {
            return 0;
        };
        let cells = OverlayRow {
            id: RowId::original(u32::try_from(row).unwrap_or(u32::MAX)),
            edits: None,
            columns,
        };
        count_matching(
            &RowView::new(parser, bytes, base, &parsed, cells),
            &self.matcher,
        )
    }

    /// Catches the counts up with the edits made since they were last right
    /// (`Found::synced`): each row those edits touched that the search has
    /// searched is recounted as it reads now (0 if it is deleted), without
    /// holding the counts' lock, then the counts are rebuilt in one pass
    /// (`Found::merge`, and `Found::merge_inserted` for inserted rows,
    /// which are all searched). If a chunk was added meanwhile, it goes
    /// round again. Returns whether the counts are caught up.
    ///
    /// With no edits since, it returns at once, taking no lock but the
    /// counts'. A main-thread query ([`CatchUp::Inline`]) does only a
    /// little: it gives up, returning `false`, if the edits since touched
    /// more than [`INLINE_ROWS`] rows (a row insert or delete counts each
    /// of its rows), or the counts have more than [`INLINE_MATCHING_ROWS`]
    /// matching rows to rebuild, or another catch-up is running. A job
    /// checkpoints every [`CATCH_UP_ROWS`] rows, and if it is cancelled it
    /// returns `false` without moving the counts on.
    fn catch_up(&self, how: CatchUp<'_>) -> Result<bool, ReadError> {
        let inline = matches!(how, CatchUp::Inline);
        self.restart_if_columns_changed();
        if !self.turning.load(Ordering::Acquire) && self.lock().restarted {
            // Started again after its job finished: a catch-up job
            // searches the rows (and catches up as it goes).
            let CatchUp::Job(job) = how else {
                return Ok(false);
            };
            self.turning.store(true, Ordering::Release);
            let polled = self.search_on(job);
            self.turning.store(false, Ordering::Release);
            // A row that couldn't be read is searched again next time.
            return Ok(matches!(polled, Poll::Ready(Ok(_))));
        }
        let version = self.reading.edits.version();
        {
            let found = self.lock();
            if found.synced == version {
                return Ok(true);
            }
            let behind = version.saturating_sub(found.synced);
            if inline && (behind > INLINE_ROWS || found.rows.len() > INLINE_MATCHING_ROWS) {
                return Ok(false);
            }
        }
        let _one_at_a_time = if inline {
            match self.catching_up.try_lock() {
                Ok(guard) => guard,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => return Ok(false),
            }
        } else {
            self.catching_up
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        loop {
            self.restart_if_columns_changed();
            let (synced, searched, matching, generation) = {
                let found = self.lock();
                (
                    found.synced,
                    found.searched,
                    found.rows.len(),
                    found.generation,
                )
            };
            let (touched, map, version) = self.reading.edits.since(synced);
            let columns = self.reading.edits.columns();
            let mut originals: Vec<(usize, Option<Arc<RowEdits>>)> = Vec::new();
            let mut inserted: Vec<(u32, InsertedNow)> = Vec::new();

            for touched in touched {
                if let Some(row) = touched.id.physical() {
                    let row = to_usize(row);
                    if row >= searched {
                        continue;
                    }
                    originals.push((row, touched.edits));
                } else if let Some(n) = touched.id.inserted_index() {
                    inserted.push((n, touched.inserted.map(|row| (row, touched.edits))));
                }
            }
            let rows = originals.len() + inserted.len();
            let big = rows > INLINE_ROWS || (rows > 0 && matching > INLINE_MATCHING_ROWS);
            if inline && big {
                return Ok(false);
            }
            let mut counts = Vec::with_capacity(originals.len());
            for (i, (row, edits)) in originals.into_iter().enumerate() {
                if let CatchUp::Job(job) = how
                    && i % CATCH_UP_ROWS == 0
                    && job.checkpoint().is_err()
                {
                    return Ok(false);
                }
                // A deleted row has no matches.
                let gone = map
                    .logical_of(u32::try_from(row).unwrap_or(u32::MAX))
                    .is_err();
                let cells = if gone {
                    0
                } else {
                    match self.row_bytes(row)? {
                        Some(RowRead { bytes, base, index }) => self.edited_cells_matching(
                            index,
                            row,
                            &bytes,
                            base,
                            edits.as_deref(),
                            &columns,
                        ),
                        None => 0,
                    }
                };
                counts.push((row, u64::from(cells)));
            }
            let parser = &self.reading.parser;
            let inserted_counts: Vec<(u32, u32, u64)> = inserted
                .into_iter()
                .map(|(n, now)| match now {
                    Some((row, edits)) => {
                        let cells = OverlayRow {
                            id: RowId::inserted(n),
                            edits: edits.as_deref(),
                            columns: &columns,
                        };
                        let matching = inserted_cells_matching(parser, &self.matcher, &row, cells);
                        (n, row.gap(), matching)
                    }
                    None => (n, 0, 0),
                })
                .collect();
            let mut found = self.lock();
            let moved = self.reading.edits.column_generation() != generation;
            if found.synced != synced || found.searched != searched || moved {
                // A chunk was added (or the search started again) meanwhile.
                continue;
            }
            found.merge(&counts);
            found.merge_inserted(&inserted_counts, map);
            found.synced = version;
            drop(found);
            if self.reading.edits.version() == version {
                return Ok(true);
            }
        }
    }

    /// Whether edits have been made since the counts were last right.
    fn behind(&self) -> bool {
        self.lock().synced != self.reading.edits.version()
    }

    /// Starts a job that catches the counts up, unless one is running or
    /// the search was dropped. The job marks itself finished even if it
    /// panics, and once it has caught up, starts again if a request came
    /// while it ran, after its last look.
    fn start_catch_up(state: &Arc<SearchState>) {
        if state.dropped.load(Ordering::Acquire)
            || state.catch_up_started.swap(true, Ordering::AcqRel)
        {
            return;
        }
        /// Clears `catch_up_started` when the job ends, however it ends.
        struct Started<'a>(&'a SearchState);
        impl Drop for Started<'_> {
            fn drop(&mut self) {
                self.0.catch_up_started.store(false, Ordering::Release);
            }
        }
        let worker = Arc::clone(state);
        let job = state
            .scheduler
            .spawn(Priority::P2, Interval::Find, move |job| {
                let caught_up = {
                    let _started = Started(&worker);
                    worker.catch_up(CatchUp::Job(job))
                };
                // A request that came after the job's last look found it
                // still running, so it didn't start another: this does.
                if matches!(caught_up, Ok(true)) && !job.is_cancelled() && worker.behind() {
                    SearchState::start_catch_up(&worker);
                }
                match caught_up {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(JobError::Cancelled),
                    Err(error) => Err(JobError::from(error)),
                }
            });
        *state
            .catch_up_job
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(job);
    }

    /// Cancels the catch-up job, if there is one.
    fn cancel_catch_up(&self) {
        if let Some(job) = self
            .catch_up_job
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            job.cancel();
        }
    }

    /// The test hook (see the field); nothing outside tests.
    #[cfg_attr(not(test), expect(clippy::unused_self))]
    fn in_chunk(&self) {
        #[cfg(test)]
        if let Some(hook) = self
            .in_chunk
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            hook();
        }
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
    fn cells_matching(&self, index: &RowIndex, row: usize, bytes: &[u8], base: usize) -> u32 {
        let parser = &self.reading.parser;
        let Some(parsed) = parser.parse_row_in(index, row, bytes, base) else {
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

    /// Hands row `slot`, as it reads now, to `each`; nothing if it can't
    /// be read (or is gone).
    fn with_view(&self, slot: Slot, each: impl FnOnce(&RowView<'_>)) -> Result<(), ReadError> {
        let parser = &self.reading.parser;
        match slot {
            Slot::Original(row) => {
                let row = to_usize(row);
                let Some(RowRead { bytes, base, index }) = self.row_bytes(row)? else {
                    return Ok(());
                };
                let Some(parsed) = parser.parse_row_in(index, row, &bytes, base) else {
                    return Ok(());
                };
                let overlay = self.reading.edits.overlay();
                each(&RowView::new(
                    parser,
                    &bytes,
                    base,
                    &parsed,
                    overlay.physical(row),
                ));
            }
            Slot::Inserted(n) => {
                let overlay = self.reading.edits.overlay();
                if let Some(row) = overlay.inserted(n) {
                    let cells = overlay.cells_of(RowId::inserted(n));
                    each(&RowView::inserted(parser, row, cells));
                }
            }
        }
        Ok(())
    }

    /// Which of row `slot`'s cells match as it reads now, in order.
    fn columns_of(&self, slot: Slot) -> Result<Vec<usize>, ReadError> {
        let mut columns = Vec::new();
        self.with_view(slot, |view| {
            // Padding is empty, and a query never is, so it never matches.
            columns = view
                .filled()
                .into_iter()
                .filter(|&(_, cell)| self.matcher.is_match(&view.value_of(cell)))
                .map(|(column, _)| column)
                .collect();
        })?;
        Ok(columns)
    }

    /// Row `row`'s bytes (line ending included), their offset in the file
    /// and the index that has the row, if one does
    /// ([`rows_index`](Self::rows_index)).
    fn row_bytes(&self, row: usize) -> Result<Option<RowRead<'_>>, ReadError> {
        let stale = self.source.changed_on_disk();
        let (index, available) = self.rows_index(stale);
        if row >= available {
            return Ok(None);
        }
        let Some(extent) = index.row_extent(row) else {
            return Ok(None);
        };
        let base = extent.start;
        Ok(Some(RowRead {
            bytes: bytes_in(&self.source, &self.head, extent, stale)?.0,
            base,
            index,
        }))
    }
}

/// The end of the chunk that starts at row `next` of `index`: about
/// [`SEARCH_CHUNK_BYTES`] of rows, at least one, at most `available`.
fn chunk_end(index: &RowIndex, next: usize, available: usize) -> usize {
    let Some(start) = index.row_extent(next).map(|extent| extent.start) else {
        return next + 1;
    };
    index
        .row_at_offset(start.saturating_add(SEARCH_CHUNK_BYTES))
        .map_or(available, |row| row.max(next + 1))
        .min(available)
}

/// One row's bytes, line ending included, where they start in the file, and
/// the index the row is in.
struct RowRead<'a> {
    bytes: Cow<'a, [u8]>,
    base: usize,
    index: &'a RowIndex,
}

/// Whether more rows can still come, for a job that has read every row it
/// can so far ([`more_rows`]).
pub(super) enum Settled {
    /// No more will come: the rows that can be read now are all there will
    /// be.
    Done,
    /// More may come: the job is woken when they do, or the index stops.
    /// End the turn (`Poll::Pending`).
    Waiting,
}

/// How the index filled by the job `filler` has ended, if it has, for a
/// reader of its rows: `Ok` if no more rows will come, because it is
/// complete or stopped by a read error (its removable drive vanished, or
/// the file changed while it was read: the rows read stay readable,
/// ADR-0006), or the job's other error (cancelled with the document or by
/// a reinterpret, or failed). `None` while it is still going.
pub(super) fn index_ended(index: &RowIndex, filler: &JobControl) -> Option<Result<(), JobError>> {
    if index.status() == Status::Complete {
        return Some(Ok(()));
    }
    match filler.outcome()? {
        Ok(()) | Err(JobError::Read(_)) => Some(Ok(())),
        // The index may have completed between the two looks.
        Err(_) if index.status() == Status::Complete => Some(Ok(())),
        Err(error) => Some(Err(error)),
    }
}

/// For a resumable job ([`Scheduler::spawn_resumable`]) that has read every
/// row it can so far, `seen` of them from `index`: whether more can come,
/// and if so, `job` is woken when they do ([`RowIndex::when_past`]) or the
/// index stops. `filler` is the job filling `index`; if it stopped other
/// than with a read error ([`index_ended`]), so does the caller, with its
/// error. No thread waits meanwhile (p1-review conc-1).
///
/// [`Scheduler::spawn_resumable`]: crate::schedule::Scheduler::spawn_resumable
pub(super) fn more_rows(
    index: &RowIndex,
    filler: &JobControl,
    seen: usize,
    job: &Job,
) -> Result<Settled, JobError> {
    match index_ended(index, filler) {
        Some(Ok(())) => return Ok(Settled::Done),
        Some(Err(error)) => return Err(error),
        None => {}
    }
    if job.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    let waker = job.waker();
    if index.status() == Status::Indexing {
        // Called at once if rows came, or the index stopped, since `seen`.
        index.when_past(seen, move || waker.wake());
    } else {
        // Stopped (or complete), but its job hasn't said how it ended yet:
        // it will in a moment.
        filler.on_finish(move || waker.wake());
    }
    Ok(Settled::Waiting)
}

fn to_usize(row: u32) -> usize {
    usize::try_from(row).unwrap_or(usize::MAX)
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{CodeUnit, IndexDialect};

    fn index(bytes: &[u8]) -> RowIndex {
        let dialect = IndexDialect {
            delimiter: b',',
            quote: b'"',
            code_unit: CodeUnit::Byte,
            bom_len: 0,
        };
        RowIndex::build(bytes, dialect).unwrap()
    }

    /// A search's chunks are about 64 KiB of rows (p1-review tests-4):
    /// task 1.8's review measured 256 KiB at up to 15 ms on rows with many
    /// candidates, over DESIGN §3.10 rule 3's ~5 ms. At least one row,
    /// however long, and never past the rows there are.
    #[test]
    fn a_search_chunk_is_about_64_kib_of_rows() {
        assert_eq!(SEARCH_CHUNK_BYTES, 64 << 10);
        let row = b"12345,abcdefghij,klmnopqrst\n";
        let bytes = row.repeat(10_000);
        let index = index(&bytes);
        let rows = index.row_count();
        for start in [0, 5_000] {
            let end = chunk_end(&index, start, rows);
            let covered = index.rows_extent(start..end).unwrap().len();
            assert!(
                (SEARCH_CHUNK_BYTES - row.len()..=SEARCH_CHUNK_BYTES).contains(&covered),
                "from row {start}: {covered} bytes"
            );
        }
        assert_eq!(chunk_end(&index, rows - 3, rows), rows);
        assert_eq!(chunk_end(&index, 0, 10), 10, "only the rows available");
        assert_eq!(chunk_end(&index, rows + 5, rows), rows + 6);

        // A row longer than a chunk is a chunk of its own.
        let long = [vec![b'x'; 100 << 10], b"\ny\nz\n".to_vec()].concat();
        let index = super::tests::index(&long);
        assert_eq!(chunk_end(&index, 0, 3), 1);
        assert_eq!(chunk_end(&index, 1, 3), 3);
    }
}
