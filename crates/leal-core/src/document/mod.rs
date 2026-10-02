//! An open document: opening a file the way DESIGN §3.10 asks, and reading
//! its rows while the background work runs.
//!
//! [`Document::open`] does the first-paint work (P0) on the calling thread
//! and returns the [`FirstScreen`] without waiting for anything else:
//!
//! 1. open the [`Source`] (clone and map, or, on a removable drive,
//!    ordinary reads);
//! 2. read the first 64 KB (one read) and [`detect()`] the encoding and
//!    dialect from it;
//! 3. index those 64 KB on their own and parse the first screen of rows
//!    from them. Their offsets are the same as in the whole file, so they
//!    serve rows until the real index has them.
//!
//! Only then does it start the background jobs, through the [`Scheduler`]:
//!
//! - **P1, the row index**, on its own thread. On an internal volume it
//!   scans the mapped file ([`Indexer::run`]). On a removable drive it
//!   scans [`Source::stream`]'s chunks ([`Indexer::chunked`]), the same
//!   pass that copies the file to the internal disk (ADR-0006), so the
//!   file is read once.
//! - **P2, the review** ([`review_with`]): the whole-file encoding and
//!   delimiter check, which can only *suggest* a change (ADR-0005 decision
//!   4). It runs alongside the index on a mapped file. On a removable drive
//!   it waits for the index pass, because it needs the whole file mapped,
//!   which happens when the copy is complete.
//! - **Diagnostics** (task 1.5) are collected by the P1 index pass itself,
//!   over the map or chunk by chunk, so they need no job of their own.
//!   The index job makes them when it starts ([`Indexer::with_diagnostics`]),
//!   on its own thread: first paint does no diagnostics work (DESIGN §3.10
//!   rule 1), and starting the jobs only makes an empty place for them.
//!   [`Document::diagnostics`] gives the report of
//!   the rows indexed so far, and [`Document::row_has_diagnostic`] and its
//!   neighbours mark every affected row.
//! - **P3**: nothing yet. Filter and sort acceleration (phase 3) will be
//!   started on first use, or when idle, with [`Priority::P3`].
//!
//! Rows are read with [`Document::rows`], synchronously, from the main
//! thread if need be: from the first 64 KB while the index hasn't reached
//! them, then from the index, with one read of the file per call (a slice
//! of the map, or one `pread` on a removable drive before its copy is
//! mapped). [`Document::reinterpret`] reads the file again with another
//! delimiter, header or encoding (**Treat as**, **Reopen with encoding…**)
//! without reopening it: it cancels the old jobs and starts new ones.
//!
//! [`Indexer::run`]: crate::index::Indexer::run
//! [`Indexer::chunked`]: crate::index::Indexer::chunked
//! [`Priority::P3`]: crate::schedule::Priority::P3

mod search;
#[cfg(test)]
mod tests;
mod values;

pub use search::{
    CellMatch, SEARCH_CHUNK_BYTES, Search, SearchProgress, SearchStep, SearchSummary,
};
pub use values::{CellValue, CopiedText, push_tsv_cell};

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError, RwLock};

use crate::detect::{
    self, ChoiceError, Choices, Detection, FIRST_PAINT_BYTES, Hints, Review, detect, review_with,
};
use crate::diagnostics::{
    DiagnosticKind, Diagnostics, Hit, Mark, Report, RowFlags, decided_by_bytes, field_with,
    next_hit, row_may_have,
};
use crate::dialect::{Encoding, QUOTE};
use crate::index::{
    DIAGNOSTICS_CHUNK_BYTES, IndexDialect, IndexError, Indexer, MAX_FILE_BYTES, Progress, RowIndex,
    Status,
};
use crate::rows::{
    DEFAULT_CACHE_ROWS, FieldSpan, NUMBER_MAX_CHARS, NumericColumns, ParsedRow, RowCache, RowParser,
};
use crate::schedule::{Interval, IntervalGuard, Job, JobError, JobHandle, Priority, Scheduler};
use crate::source::{
    OpenError, Original, OriginalState, OriginalStatus, ReadError, ReadErrorKind, Source, Storage,
    TempFolders, VolumeInfo,
};

/// Where an occurrence of a diagnostic is, for the details popover's
/// **Previous** and **Next** (task 1.7): the cell to select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// The 0-based physical row (the header row, if any, is row 0).
    pub row: usize,
    /// The 0-based field: the one with the occurrence, or for a ragged row
    /// its first missing or extra cell.
    pub column: usize,
}

/// Which way [`Document::next_with_kind`] and its neighbour search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    Forward,
    Backward,
}

/// The kinds a row mark's flag stands for (see `diagnostics::marks`).
const FLAGGED_KINDS: [DiagnosticKind; 4] = [
    DiagnosticKind::UnterminatedQuote,
    DiagnosticKind::TextAfterClosingQuote,
    DiagnosticKind::InvalidEncoding,
    DiagnosticKind::NulBytes,
];

/// One row's bytes, read for a kind's search (`Document::row_bytes`).
struct RowBytes<'a, 'r> {
    /// The row, line ending included.
    bytes: Cow<'a, [u8]>,
    /// Where `bytes` start in the file.
    base: usize,
    /// The index the row is in (the first 64 KB's, or the file's).
    index: &'r RowIndex,
}

/// How many candidate rows a kind's search takes from the row marks per
/// lock.
const SEARCH_BATCH: usize = 256;

/// The row of the occurrence of `kind` that **Next** from `start`
/// (`Forward`) or **Previous** before it would find, if the report's
/// locations decide it. They list every occurrence in file order up to the
/// last one listed, so a listed row past `start` is the next one, and
/// before `start` the nearest listed row is the previous one when `start`
/// is within the list or the list is complete.
fn listed_row(
    report: &Report,
    kind: DiagnosticKind,
    start: usize,
    direction: Direction,
) -> Option<usize> {
    let diagnostic = report.get(kind)?;
    let rows = diagnostic.first();
    // The locations are in file order: the first one at or after `start`.
    let at = rows.partition_point(|location| location.row < start);
    match direction {
        Direction::Forward => rows.get(at).map(|location| location.row),
        Direction::Backward => {
            let last = rows.last()?.row;
            let complete = rows.len() == diagnostic.count();
            if start > last && !complete {
                return None;
            }
            Some(rows.get(at.checked_sub(1)?)?.row)
        }
    }
}

/// How to open a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenOptions {
    /// The user's delimiter, header or encoding, in place of detection's.
    pub choices: Choices,
    /// How many rows the first screen has (the header row included).
    pub first_screen_rows: usize,
    /// The most characters of each cell the first screen shows (see
    /// [`RowParser::display_prefix`]).
    pub max_chars: usize,
}

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions {
            choices: Choices::default(),
            first_screen_rows: 100,
            max_chars: 256,
        }
    }
}

/// One cell, as the grid shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    /// The start of the cell's display value.
    pub text: String,
    /// Whether the value has more than `text`.
    pub truncated: bool,
}

/// One row's cells in a window of columns ([`Document::cells`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowCells {
    /// How many fields the whole row has, inside the window or not. The
    /// grid uses it to tell a short row's missing cells from empty ones.
    pub field_count: usize,
    /// The row's cells in the window: fewer if the row ends inside it, none
    /// if it ends before it.
    pub cells: Vec<Cell>,
}

/// What first paint (P0) found: how to read the file, and its first rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstScreen {
    /// Which reading of the file this is: it goes up by one with each
    /// [`Document::reinterpret`], and progress reports carry it.
    pub generation: u64,
    /// How the file is read.
    pub detection: Detection,
    /// The first rows, each as its cells.
    pub rows: Vec<Vec<Cell>>,
    /// Rows known so far: those in the first 64 KB.
    pub row_count: usize,
    /// The row count to size the scrollbar with (DESIGN §3.10 rule 5): the
    /// file's size over the first 64 KB's average row length, or the exact
    /// count if the file fits in 64 KB.
    pub estimated_row_count: usize,
    /// The most common field count in the first 64 KB (ADR-0003 decision
    /// 4): the grid's column count until [`Document::column_count`] has
    /// the index's. 0 for an empty file.
    pub column_count: usize,
}

/// Where indexing has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexProgress {
    /// The reading this is about ([`FirstScreen::generation`]).
    pub generation: u64,
    /// Rows that can be read now.
    pub rows: usize,
    /// The row count to size the scrollbar with: exact once `complete`.
    pub estimated_rows: usize,
    /// How far the index has got, in bytes.
    pub bytes_scanned: u64,
    /// The file's length.
    pub bytes_total: u64,
    /// Whether every row is indexed.
    pub complete: bool,
}

/// What the finished index found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexSummary {
    /// The number of rows.
    pub rows: usize,
    /// The most common field count among non-blank rows (ADR-0003
    /// decision 4).
    pub field_count_mode: Option<usize>,
    /// The opening quote of a quoted field that never closes, if any.
    pub unterminated_quote: Option<usize>,
}

/// Told about indexing progress, on the index's thread, after each chunk
/// (about every millisecond). Keep it short: forward to the main thread.
pub type ProgressCallback = Arc<dyn Fn(IndexProgress) + Send + Sync>;

/// Told when the user's file changes, moves, is deleted or its volume goes
/// ([`Document::watch_original`]), on the watching thread. Keep it short:
/// forward to the main thread.
pub type OriginalCallback = Arc<dyn Fn(&OriginalStatus) + Send + Sync>;

/// Why a document couldn't be opened or re-read.
#[derive(Debug)]
pub enum DocumentError {
    /// The file couldn't be opened.
    Open(OpenError),
    /// The start of the file couldn't be read (a removable drive that
    /// vanished straight after opening).
    Read(ReadError),
    /// The user's encoding doesn't fit the file's BOM.
    Choice(ChoiceError),
    /// The file is larger than Leal reads ([`MAX_FILE_BYTES`], DESIGN §1).
    TooLarge {
        /// The file's length.
        len: u64,
    },
    /// Detection gave a dialect the index or the row parser refused. This
    /// is a bug; the message is English, for logs.
    Internal(String),
}

impl fmt::Display for DocumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocumentError::Open(error) => error.fmt(f),
            DocumentError::Read(error) => error.fmt(f),
            DocumentError::Choice(error) => error.fmt(f),
            DocumentError::TooLarge { len } => write!(
                f,
                "the file is {len} bytes; Leal reads files of up to {MAX_FILE_BYTES} bytes"
            ),
            DocumentError::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DocumentError {}

impl From<OpenError> for DocumentError {
    fn from(error: OpenError) -> Self {
        DocumentError::Open(error)
    }
}

impl From<ReadError> for DocumentError {
    fn from(error: ReadError) -> Self {
        DocumentError::Read(error)
    }
}

impl From<ChoiceError> for DocumentError {
    fn from(error: ChoiceError) -> Self {
        DocumentError::Choice(error)
    }
}

/// An open file, read the way detection (or the user) says. It is `Send`
/// and `Sync`: the app shares one between the main thread, which reads
/// rows, and the jobs that fill it. Dropping it cancels its jobs; the
/// file's clone or copy is deleted once they have stopped.
pub struct Document {
    source: Arc<Source>,
    scheduler: Scheduler,
    /// The first 64 KB, read once at first paint and kept: re-reading the
    /// file with other choices starts from them, and rows in them can be
    /// read even if a removable drive vanishes before they were copied.
    head: Arc<[u8]>,
    progress: Option<ProgressCallback>,
    /// The next reading's generation.
    generations: AtomicU64,
    /// Held by `reinterpret`, so two at once can't leave a reading whose
    /// jobs nobody cancels.
    reinterpreting: Mutex<()>,
    /// Counts kind searches (`next_with_kind`): a search stops when a
    /// newer one starts.
    searches: AtomicU64,
    reading: RwLock<Arc<Reading>>,
    /// The user's file, watched for changes made elsewhere (task 1.9).
    original: Original,
}

/// One reading of the file: a detection, and the index and jobs built
/// from it. [`Document::reinterpret`] replaces it.
struct Reading {
    generation: u64,
    /// The user's choices it was read with, so it can be read again the
    /// same way ([`Document::check_original`] after a drive comes back).
    choices: Choices,
    detection: Detection,
    parser: RowParser,
    /// The first 64 KB on their own, for rows before the index has them.
    head_index: RowIndex,
    /// How many of `head_index`'s rows are whole: all but the last, which
    /// may be cut, unless the file fits in 64 KB.
    head_rows: usize,
    index: Arc<RowIndex>,
    /// What the index pass finds wrong with the file, so far. Empty until
    /// the index job starts and makes them, so that first paint doesn't.
    diagnostics: DiagnosticsSlot,
    index_job: JobHandle<IndexSummary>,
    review_job: JobHandle<Review>,
    cache: Mutex<RowCache>,
    /// Set once the row cache has been emptied after the file changed
    /// while it was read ([`Document::rows`]).
    cache_dropped: AtomicBool,
}

/// First paint's result (P0), before any job starts.
struct FirstPaint {
    detection: Detection,
    parser: RowParser,
    head_index: RowIndex,
    head_rows: usize,
    /// The real index, empty, and the indexer that will fill it. The
    /// indexer has no diagnostics yet: the index job adds them.
    index: Arc<RowIndex>,
    indexer: Indexer,
}

/// Where the index job puts a reading's [`Diagnostics`] once it has made
/// them, for the reading's readers.
type DiagnosticsSlot = Arc<OnceLock<Arc<Diagnostics>>>;

/// The report readers get before the index job has made the diagnostics:
/// empty and incomplete, as [`Diagnostics::report`] is before the first
/// chunk. Made on first use, by a reader, never by first paint.
static NO_REPORT: LazyLock<Arc<Report>> = LazyLock::new(Arc::default);

/// What the jobs of every reading of a document share.
struct Context<'a> {
    source: &'a Arc<Source>,
    scheduler: &'a Scheduler,
    progress: Option<&'a ProgressCallback>,
}

impl Document {
    /// Opens the file at `path` and reads its first screen (P0), then
    /// starts indexing (P1) and the review (P2). It returns as soon as the
    /// first screen is ready, without waiting for either job.
    ///
    /// `temp` and `volume` are as for [`Source::open_on`]. `progress`, if
    /// given, is told about indexing progress.
    ///
    /// # Errors
    ///
    /// [`DocumentError::Open`] if the file can't be opened,
    /// [`DocumentError::Read`] if its start can't be read,
    /// [`DocumentError::Choice`] if the chosen encoding doesn't fit its
    /// BOM, or [`DocumentError::TooLarge`].
    pub fn open(
        path: &Path,
        temp: &TempFolders,
        volume: VolumeInfo,
        scheduler: &Scheduler,
        options: OpenOptions,
        progress: Option<ProgressCallback>,
    ) -> Result<(Document, FirstScreen), DocumentError> {
        let first_paint = scheduler.interval(Interval::FirstPaint);
        let source = Source::open_on(path, temp, volume)?;
        Document::start(source, scheduler, options, progress, first_paint)
    }

    /// [`open`](Self::open), for a [`Source`] already opened.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open), except [`DocumentError::Open`].
    pub fn from_source(
        source: Source,
        scheduler: &Scheduler,
        options: OpenOptions,
        progress: Option<ProgressCallback>,
    ) -> Result<(Document, FirstScreen), DocumentError> {
        let first_paint = scheduler.interval(Interval::FirstPaint);
        Document::start(source, scheduler, options, progress, first_paint)
    }

    fn start(
        source: Source,
        scheduler: &Scheduler,
        options: OpenOptions,
        progress: Option<ProgressCallback>,
        first_paint: IntervalGuard,
    ) -> Result<(Document, FirstScreen), DocumentError> {
        let len = source.len();
        if usize::try_from(len).map_or(true, |len| len > MAX_FILE_BYTES) {
            return Err(DocumentError::TooLarge { len });
        }
        // The one read first paint makes. On an internal volume it is a
        // slice of the map; the copy costs a few microseconds.
        let head: Arc<[u8]> = Arc::from(&*source.read_range(0..FIRST_PAINT_BYTES)?);
        let source = Arc::new(source);
        let paint = read_first_paint(&source, &head, options.choices)?;
        let screen = first_screen(&head, len, 0, &paint, options);
        // First paint is done: everything after this is background work.
        drop(first_paint);
        let context = Context {
            source: &source,
            scheduler,
            progress: progress.as_ref(),
        };
        let reading = start_jobs(&context, 0, paint, options.choices);
        // One look at the user's file now (a few system calls), so a change
        // between opening it and here isn't missed; watching it starts when
        // the app asks (`watch_original`).
        let original = Original::new(source.path(), *source.identity());
        let document = Document {
            scheduler: scheduler.clone(),
            head,
            progress,
            generations: AtomicU64::new(1),
            reinterpreting: Mutex::new(()),
            searches: AtomicU64::new(0),
            reading: RwLock::new(Arc::new(reading)),
            source,
            original,
        };
        Ok((document, screen))
    }

    /// Reads the file again with `choices` in place of what detection
    /// found (**Treat as**, the header toggle, **Reopen with encoding…**),
    /// without reopening it: first paint again from the first 64 KB, then
    /// a new index and review. The old reading's jobs are cancelled. Rows
    /// read before this belong to the old reading; read them again.
    ///
    /// # Errors
    ///
    /// [`DocumentError::Choice`] if the chosen encoding doesn't fit the
    /// file's BOM. The document is then unchanged.
    pub fn reinterpret(
        &self,
        choices: Choices,
        first_screen_rows: usize,
        max_chars: usize,
    ) -> Result<FirstScreen, DocumentError> {
        let _one_at_a_time = self
            .reinterpreting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let first_paint = self.scheduler.interval(Interval::FirstPaint);
        let paint = read_first_paint(&self.source, &self.head, choices)?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let options = OpenOptions {
            choices,
            first_screen_rows,
            max_chars,
        };
        let screen = first_screen(&self.head, self.source.len(), generation, &paint, options);
        drop(first_paint);
        // Stop the old jobs first: a removable drive's stream is one pass
        // at a time, so the new index waits for the old one to stop.
        self.current().cancel();
        let context = Context {
            source: &self.source,
            scheduler: &self.scheduler,
            progress: self.progress.as_ref(),
        };
        let reading = start_jobs(&context, generation, paint, choices);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(reading);
        Ok(screen)
    }

    /// Reads the file again the way the current reading does, with a new
    /// index and review and a new generation: after its removable drive
    /// came back, so the index pass carries on copying it. The first screen
    /// is the same as before.
    fn restart(&self) -> Result<u64, DocumentError> {
        let _one_at_a_time = self
            .reinterpreting
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let old = self.current();
        let paint = read_first_paint(&self.source, &self.head, old.choices)?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        old.cancel();
        let context = Context {
            source: &self.source,
            scheduler: &self.scheduler,
            progress: self.progress.as_ref(),
        };
        let reading = start_jobs(&context, generation, paint, old.choices);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(reading);
        Ok(generation)
    }

    /// Rows `rows` (as many of them as can be read now), each as its
    /// cells, with at most `max_chars` characters of each. Fast enough for
    /// the main thread: it reads the rows' bytes once, as one range, and
    /// keeps recently parsed rows in a small cache.
    ///
    /// Rows the index hasn't reached yet, beyond those in the first 64 KB,
    /// aren't returned: the result is shorter, or empty.
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if the bytes can't be read. That happens only for a
    /// file on a removable drive whose copy isn't complete: for example,
    /// rows that weren't copied before the drive vanished
    /// ([`ReadErrorKind::Disconnected`]).
    pub fn rows(&self, rows: Range<usize>, max_chars: usize) -> Result<Vec<Vec<Cell>>, ReadError> {
        self.read_rows(rows, |parser, bytes, base, row| {
            cells(parser, bytes, base, row.fields(), max_chars)
        })
    }

    /// Rows `rows`, as for [`rows`](Self::rows), but only their cells in
    /// the columns `columns`, with each row's whole field count. The grid
    /// reads what it shows this way, so a wide file costs no more to draw
    /// than the columns on screen (the 1.4 notes' open question): each row
    /// is still split into fields, but only the window's cells are decoded
    /// and copied.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn cells(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
        max_chars: usize,
    ) -> Result<Vec<RowCells>, ReadError> {
        self.read_rows(rows, |parser, bytes, base, row| {
            let fields = row.fields();
            let window = fields
                .get(
                    columns.start.min(fields.len())..columns.end.clamp(columns.start, fields.len()),
                )
                .unwrap_or_default();
            RowCells {
                field_count: fields.len(),
                cells: cells(parser, bytes, base, window, max_chars),
            }
        })
    }

    /// The grid's column count: the most common field count among the
    /// rows read so far (ADR-0003 decision 4), from the first 64 KB until
    /// the index has passed them. It can change while indexing and is
    /// final once the index is complete. 0 for an empty file.
    #[must_use]
    pub fn column_count(&self) -> usize {
        let reading = self.current();
        self.rows_index(&reading).0.field_count_mode().unwrap_or(0)
    }

    /// Which columns hold numbers, from the first `sample` rows after the
    /// header row (see [`NumericColumns`]): one entry per column of the
    /// longest row in the sample. The grid right-aligns them (DESIGN §4.1).
    /// First paint asks about the first screen; the 1,000-row sample is P2
    /// work (§3.10), so the app asks for it off the main thread.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn numeric_columns(&self, sample: usize) -> Result<Vec<bool>, ReadError> {
        let first = usize::from(self.current().detection.header);
        let mut columns = NumericColumns::new();
        self.read_rows(
            first..first.saturating_add(sample),
            |parser, bytes, base, row| {
                for (column, field) in row.fields().iter().enumerate() {
                    let (text, truncated) =
                        parser.display_prefix_in(bytes, base, field, NUMBER_MAX_CHARS);
                    columns.add(column, &text, truncated);
                }
            },
        )?;
        Ok(columns.result())
    }

    /// Reads rows `rows` (as many as can be read now) and hands each to
    /// `each`, with the parser, the bytes holding them and the bytes'
    /// offset in the file. One read of the file per call.
    fn read_rows<T>(
        &self,
        rows: Range<usize>,
        each: impl FnMut(&RowParser, &[u8], usize, &ParsedRow) -> T,
    ) -> Result<Vec<T>, ReadError> {
        self.read_rows_of(&self.current(), rows, each)
    }

    /// [`read_rows`](Self::read_rows), in a given reading.
    fn read_rows_of<T>(
        &self,
        reading: &Reading,
        rows: Range<usize>,
        mut each: impl FnMut(&RowParser, &[u8], usize, &ParsedRow) -> T,
    ) -> Result<Vec<T>, ReadError> {
        let (index, available) = self.rows_index(reading);
        let rows = rows.start..rows.end.min(available);
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok(Vec::new());
        };
        let bytes = self.bytes_of(extent.clone())?;
        let base = extent.start;
        let mut cache = reading.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if self.head_is_stale() && !reading.cache_dropped.swap(true, Ordering::AcqRel) {
            // Rows parsed before the change was noticed may be from either
            // version of the file.
            cache.clear();
        }
        Ok(rows
            .filter_map(|r| {
                let row = cache.row_in(index, r, &bytes, base)?;
                Some(each(&reading.parser, &bytes, base, &row))
            })
            .collect())
    }

    /// The number of rows that can be read now: the index's so far, or the
    /// first 64 KB's if the index hasn't got that far. Once indexing is
    /// complete, the file's row count. After the file changed while it was
    /// read ([`changed_on_disk`](Self::changed_on_disk)), only the index's.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows_index(&self.current()).1
    }

    /// The index rows are read from, and how many rows it has: whichever
    /// of the two indexes covers more of the file's first rows (both cover
    /// a prefix), except that once the file changed while it was read, only
    /// the real index.
    fn rows_index<'r>(&self, reading: &'r Reading) -> (&'r RowIndex, usize) {
        let indexed = reading.index.row_count();
        if indexed >= reading.head_rows || self.head_is_stale() {
            (&*reading.index, indexed)
        } else {
            (&reading.head_index, reading.head_rows)
        }
    }

    /// The bytes of `extent`: from the first 64 KB kept in memory if they
    /// hold it, otherwise one read of the file.
    fn bytes_of(&self, extent: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
        match self.head.get(extent.clone()) {
            Some(bytes) if !self.head_is_stale() => Ok(Cow::Borrowed(bytes)),
            _ => self.source.read_range(extent),
        }
    }

    /// Whether the first 64 KB kept in memory may be a different version
    /// from the rest (task 1.9). The file changed while it was read without
    /// a snapshot: first paint read the 64 KB before the copy began, and a
    /// change that kept the size and modification time could have come in
    /// between, so from then on rows come only from the index pass's copy,
    /// whose every chunk was checked before it was kept.
    fn head_is_stale(&self) -> bool {
        self.source.changed_on_disk()
    }

    /// The row count to size the scrollbar with while indexing (DESIGN
    /// §3.10 rule 5): exact once indexing is complete.
    #[must_use]
    pub fn estimated_row_count(&self) -> usize {
        estimated_rows(&self.current(), &self.head, self.source.len())
    }

    /// Where indexing has got to.
    #[must_use]
    pub fn progress(&self) -> IndexProgress {
        let reading = self.current();
        let index = &reading.index;
        IndexProgress {
            generation: reading.generation,
            rows: self.rows_index(&reading).1,
            estimated_rows: estimated_rows(&reading, &self.head, self.source.len()),
            bytes_scanned: u64::try_from(index.bytes_scanned()).unwrap_or(u64::MAX),
            bytes_total: self.source.len(),
            complete: index.status() == Status::Complete,
        }
    }

    /// How the file is read now.
    #[must_use]
    pub fn detection(&self) -> Detection {
        self.current().detection.clone()
    }

    /// The current reading's generation ([`FirstScreen::generation`]).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.current().generation
    }

    /// The index job (P1) of the current reading.
    #[must_use]
    pub fn index_job(&self) -> JobHandle<IndexSummary> {
        self.current().index_job.clone()
    }

    /// The review job (P2) of the current reading: its result holds the
    /// encoding and delimiter suggestions.
    #[must_use]
    pub fn review_job(&self) -> JobHandle<Review> {
        self.current().review_job.clone()
    }

    /// The file's bytes.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// Where the file's bytes are held ([`Source::storage`]): for a file on
    /// a removable drive, this changes from [`Storage::Reading`] to
    /// [`Storage::Copy`] when the index pass has copied it.
    #[must_use]
    pub fn storage(&self) -> Storage {
        self.source.storage()
    }

    /// What the index pass has found wrong with the file so far
    /// (DESIGN §3.5): the report of the rows indexed so far, complete once
    /// indexing is. It is the current reading's, so after
    /// [`reinterpret`](Self::reinterpret) it starts again, empty. Shared,
    /// so reading it copies nothing.
    #[must_use]
    pub fn diagnostics(&self) -> Arc<Report> {
        self.current().report()
    }

    /// [`diagnostics`](Self::diagnostics) with the generation of the
    /// reading they belong to, both from the same reading even if a
    /// [`reinterpret`](Self::reinterpret) happens meanwhile.
    #[must_use]
    pub fn diagnostics_with_generation(&self) -> (u64, Arc<Report>) {
        let reading = self.current();
        (reading.generation, reading.report())
    }

    /// True if row `row` has a warning or an error, for its gutter marker
    /// (every such row, not only the report's first locations). False for a
    /// row not indexed yet.
    #[must_use]
    pub fn row_has_diagnostic(&self, row: usize) -> bool {
        self.current()
            .diagnostics
            .get()
            .is_some_and(|diagnostics| diagnostics.row_has_diagnostic(row))
    }

    /// The first row at or after `from` with a warning or an error, for
    /// **Next**.
    #[must_use]
    pub fn next_row_with_diagnostic(&self, from: usize) -> Option<usize> {
        self.current()
            .diagnostics
            .get()?
            .next_row_with_diagnostic(from)
    }

    /// The last row before `to` with a warning or an error, for
    /// **Previous**.
    #[must_use]
    pub fn previous_row_with_diagnostic(&self, to: usize) -> Option<usize> {
        self.current()
            .diagnostics
            .get()?
            .previous_row_with_diagnostic(to)
    }

    /// Each of rows `rows`' marks (task 1.7): whether its gutter has a
    /// marker, and whether it is ragged, so its missing cells are hatched
    /// (ADR-0002 questions 5 and 7). Rows not indexed yet are unmarked.
    #[must_use]
    pub fn row_flags(&self, rows: Range<usize>) -> Vec<RowFlags> {
        match self.current().diagnostics.get() {
            Some(diagnostics) => diagnostics.row_flags(rows),
            None => vec![RowFlags::default(); rows.len()],
        }
    }

    /// The first occurrence of `kind` in a row at or after `from`, for
    /// that kind's **Next** in the details popover (task 1.7): its row and
    /// the column to select. Every row, not only the report's first
    /// [`MAX_LOCATIONS`](crate::diagnostics::MAX_LOCATIONS): the row marks
    /// find candidate rows, and a row with several field-level kinds is
    /// checked for this one. `None` for the info-level kinds, which have
    /// no navigation (mockup 03b), and past the last occurrence indexed so
    /// far.
    ///
    /// It reads each candidate row, so it may take a while in a file where
    /// most rows have some other field-level kind: call it off the main
    /// thread.
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if a candidate row can't be read (see
    /// [`rows`](Self::rows)).
    pub fn next_with_kind(
        &self,
        kind: DiagnosticKind,
        from: usize,
    ) -> Result<Option<Place>, ReadError> {
        self.step_to_kind(kind, from, Direction::Forward)
    }

    /// The last occurrence of `kind` in a row before `to`, for **Previous**.
    /// As for [`next_with_kind`](Self::next_with_kind).
    ///
    /// # Errors
    ///
    /// As for [`next_with_kind`](Self::next_with_kind).
    pub fn previous_with_kind(
        &self,
        kind: DiagnosticKind,
        to: usize,
    ) -> Result<Option<Place>, ReadError> {
        self.step_to_kind(kind, to, Direction::Backward)
    }

    fn step_to_kind(
        &self,
        kind: DiagnosticKind,
        start: usize,
        direction: Direction,
    ) -> Result<Option<Place>, ReadError> {
        // A newer search (another click on Previous or Next) stops this one.
        let search = self
            .searches
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let reading = self.current();
        let Some(diagnostics) = reading.diagnostics.get() else {
            return Ok(None);
        };
        let report = diagnostics.report();
        self.search_kind(
            &reading,
            diagnostics,
            &report,
            kind,
            start,
            direction,
            search,
        )
    }

    /// [`step_to_kind`](Self::step_to_kind), with the report it trusts
    /// passed in, so a test can pass an older one.
    ///
    /// 1. **The report's locations.** They are every occurrence up to the
    ///    last one listed, so if one lies past `start` (Next), or `start`
    ///    is within them (Previous), it is the answer, and only its row is
    ///    read, for the column.
    /// 2. **The bytes.** For NULs and invalid UTF-8 going forward over a
    ///    mapped file, one fast search of the file from the row on finds
    ///    the next one, however many rows have other kinds.
    /// 3. **The row marks.** Otherwise candidate rows come from the marks,
    ///    a batch per lock, and each is checked from its bytes; only text
    ///    after a quote needs the row parsed, and only if it has a quote.
    ///    No row goes through the grid's row cache.
    ///
    /// Shortcuts 1 and "every flagged row has this kind" are used only once
    /// the report is complete: while indexing, the report and the marks are
    /// read at different moments, and a kind found in between would be
    /// missed.
    #[allow(clippy::too_many_arguments)]
    fn search_kind(
        &self,
        reading: &Reading,
        diagnostics: &Diagnostics,
        report: &Report,
        kind: DiagnosticKind,
        start: usize,
        direction: Direction,
        search: u64,
    ) -> Result<Option<Place>, ReadError> {
        let which = match kind {
            DiagnosticKind::RaggedRows => Mark::Ragged,
            DiagnosticKind::UnterminatedQuote
            | DiagnosticKind::TextAfterClosingQuote
            | DiagnosticKind::InvalidEncoding
            | DiagnosticKind::NulBytes => Mark::Flagged,
            DiagnosticKind::MixedLineEndings
            | DiagnosticKind::BlankLines
            | DiagnosticKind::BomPresent => return Ok(None),
        };
        let settled = report.is_complete();
        let stopped = || self.searches.load(Ordering::Relaxed) != search;

        // 1. The report's locations.
        if settled && let Some(row) = listed_row(report, kind, start, direction) {
            return self.place(reading, kind, row);
        }

        // 2. The bytes.
        let encoding = reading.detection.encoding;
        if direction == Direction::Forward
            && let Some(bytes) = self.source.as_slice()
            && let Some(extent) = reading.index.row_extent(start)
        {
            match next_hit(kind, encoding, bytes, extent.start, bytes.len(), &stopped) {
                Hit::At(offset) => match reading.index.row_at_offset(offset) {
                    Some(row) if diagnostics.row_is(row, Mark::Flagged) => {
                        return self.place(reading, kind, row);
                    }
                    // Past the rows indexed so far: none before it.
                    None => return Ok(None),
                    // Indexed, but its marks not published yet (the index
                    // publishes its rows a moment before the diagnostics):
                    // ask the marks instead.
                    Some(_) => {}
                },
                Hit::None => return Ok(None),
                Hit::Unsupported if stopped() => return Err(ReadError::cancelled()),
                Hit::Unsupported => {}
            }
        }

        // 3. The row marks. If no other flagged kind was found, every
        // flagged row has this one.
        let alone = which == Mark::Ragged
            || (settled
                && FLAGGED_KINDS
                    .iter()
                    .all(|&other| other == kind || report.get(other).is_none()));
        let forward = direction == Direction::Forward;
        let mut at = start;
        loop {
            if stopped() {
                return Err(ReadError::cancelled());
            }
            let rows = diagnostics.rows_where(at, which, forward, SEARCH_BATCH);
            let Some(&last) = rows.last() else {
                return Ok(None);
            };
            for row in rows {
                if alone || self.row_has(reading, kind, row)? {
                    return self.place(reading, kind, row);
                }
            }
            // `rows_where` looks before `at` going backward.
            at = if forward { last + 1 } else { last };
        }
    }

    /// The bytes of row `row`, line ending included, from whichever index
    /// holds it, without the row cache.
    fn row_bytes<'r>(
        &self,
        reading: &'r Reading,
        row: usize,
    ) -> Result<Option<RowBytes<'_, 'r>>, ReadError> {
        let index = if row < reading.index.row_count() {
            &*reading.index
        } else if row < reading.head_rows && !self.head_is_stale() {
            &reading.head_index
        } else {
            return Ok(None);
        };
        let Some(extent) = index.row_extent(row) else {
            return Ok(None);
        };
        let bytes = self.bytes_of(extent.clone())?;
        Ok(Some(RowBytes {
            bytes,
            base: extent.start,
            index,
        }))
    }

    /// Whether row `row` has an occurrence of the field-level `kind`.
    fn row_has(
        &self,
        reading: &Reading,
        kind: DiagnosticKind,
        row: usize,
    ) -> Result<bool, ReadError> {
        let Some(RowBytes { bytes, base, index }) = self.row_bytes(reading, row)? else {
            return Ok(false);
        };
        let encoding = reading.detection.encoding;
        if !row_may_have(kind, encoding, &bytes) {
            return Ok(false);
        }
        if decided_by_bytes(kind) {
            return Ok(true);
        }
        Ok(reading
            .parser
            .parse_row_in(index, row, &bytes, base)
            .is_some_and(|parsed| field_with(kind, encoding, &bytes, base, &parsed).is_some()))
    }

    /// Where `kind` is in `row`, which has it: the row, and the column to
    /// select. For a ragged row, its first missing cell (a short row) or
    /// first extra one (a long row); otherwise the first field with the
    /// kind.
    fn place(
        &self,
        reading: &Reading,
        kind: DiagnosticKind,
        row: usize,
    ) -> Result<Option<Place>, ReadError> {
        let column = match self.row_bytes(reading, row)? {
            Some(RowBytes { bytes, base, index }) => reading
                .parser
                .parse_row_in(index, row, &bytes, base)
                .and_then(|parsed| {
                    if kind == DiagnosticKind::RaggedRows {
                        let fields = parsed.fields().len();
                        let mode = reading.index.field_count_mode();
                        Some(mode.map_or(fields, |mode| fields.min(mode)))
                    } else {
                        field_with(kind, reading.detection.encoding, &bytes, base, &parsed)
                    }
                }),
            None => None,
        };
        Ok(Some(Place {
            row,
            column: column.unwrap_or(0),
        }))
    }

    /// Whether Save (writing over the original) is possible: `false` once
    /// the file's removable drive was disconnected before it was copied, or
    /// once the file changed while it was read without a snapshot
    /// ([`Source::can_save`]), and while the file's volume isn't mounted
    /// ([`OriginalState::Unavailable`]). Save As is always allowed
    /// (ADR-0006). A file that changed elsewhere can still be saved: Save
    /// asks first (phase 2, from [`OriginalStatus::diverged`]).
    #[must_use]
    pub fn can_save(&self) -> bool {
        self.source.can_save() && self.original.status().state != OriginalState::Unavailable
    }

    /// The user's file as last seen (task 1.9): unchanged, changed,
    /// deleted, or on a volume that isn't mounted, and where it is now.
    /// This makes no system calls.
    #[must_use]
    pub fn original(&self) -> OriginalStatus {
        self.original.status()
    }

    /// Starts watching the user's file (task 1.9): `on_change` is called on
    /// a thread of the document's own, with the new status, each time it
    /// changes. A write to a file read without a snapshot (a removable
    /// drive that can't clone) is also a change while reading
    /// ([`changed_on_disk`](Self::changed_on_disk)). Calling it again does
    /// nothing. The thread stops when the document is dropped.
    ///
    /// # Errors
    ///
    /// If the kernel's event queue or the thread can't be made; the
    /// document works without them, and
    /// [`check_original`](Self::check_original) still notices changes.
    pub fn watch_original(&self, on_change: OriginalCallback) -> std::io::Result<()> {
        // Weak: dropping the document mustn't wait for the watching thread
        // (it may be in a slow look at the file), and the thread mustn't
        // keep the file's clone or copy alive once the document is gone.
        let source = Arc::downgrade(&self.source);
        self.original.watch(move |status| {
            if status.written
                && let Some(source) = source.upgrade()
            {
                source.note_original_written();
            }
            on_change(status);
        })
    }

    /// Looks at the user's file now (task 1.9), and returns what it found.
    /// The app calls it when a volume mounts and when it becomes active.
    ///
    /// If the file's removable drive is back after it was disconnected
    /// before the copy was complete ([`Storage::Disconnected`]), and the
    /// file is unchanged, the source reconnects to the drive
    /// ([`Source::reconnect`]) and the file is read again the same way: a
    /// new index pass, with a new [`generation`](Self::generation), carries
    /// on copying it, and Save is allowed again. If it changed while the
    /// drive was away, it stays disconnected, and the status says
    /// [`OriginalState::Changed`], for the app to offer Reload.
    ///
    /// It makes a few system calls, which can block on a network volume:
    /// call it off the main thread.
    pub fn check_original(&self) -> OriginalStatus {
        let status = self.original.check();
        if status.written {
            self.source.note_original_written();
        }
        let back = matches!(
            status.state,
            OriginalState::Unchanged | OriginalState::Changed
        );
        if back
            && !status.diverged
            && self.source.storage() == Storage::Disconnected
            && self.source.reconnect(&status.path)
        {
            // Detection already succeeded with these choices on these
            // bytes, so this can't fail; if it did, the source stays
            // reconnected and the next check tries again.
            let _ = self.restart();
        }
        status
    }

    /// Whether the file changed on its drive while Leal was reading it
    /// ([`Source::changed_on_disk`]): the rows held may mix two versions.
    #[must_use]
    pub fn changed_on_disk(&self) -> bool {
        self.source.changed_on_disk()
    }

    /// The current reading. A clone of the `Arc`, so the lock is held only
    /// for a moment, and a [`reinterpret`](Self::reinterpret) meanwhile
    /// doesn't pull it away from a caller using it.
    fn current(&self) -> Arc<Reading> {
        // Only a whole `Arc` is ever stored, so a poisoned lock still holds
        // a good value.
        Arc::clone(&self.reading.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        self.current().cancel();
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("source", &self.source)
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

impl Reading {
    /// The latest diagnostics report, or an empty one if the index job
    /// hasn't made the diagnostics yet.
    fn report(&self) -> Arc<Report> {
        match self.diagnostics.get() {
            Some(diagnostics) => diagnostics.report(),
            None => Arc::clone(&NO_REPORT),
        }
    }

    /// Stops the reading's jobs.
    fn cancel(&self) {
        self.index_job.cancel();
        self.review_job.cancel();
    }
}

/// P0: detection from the first 64 KB, and the index of those 64 KB.
fn read_first_paint(
    source: &Source,
    head: &[u8],
    choices: Choices,
) -> Result<FirstPaint, DocumentError> {
    let detection = detect(
        head,
        source.len(),
        Hints::from(source.attributes()),
        choices,
    )?;
    let dialect = IndexDialect {
        delimiter: detection.delimiter.byte(),
        quote: QUOTE,
        code_unit: detection.encoding.code_unit(),
        bom_len: detection.bom.len(),
    };
    // Detection always gives a dialect both accept: a delimiter of the
    // four, `"`, a code unit that matches the encoding, and a BOM it found
    // in these bytes.
    let parser = RowParser::new(dialect, detection.encoding)
        .map_err(|error| DocumentError::Internal(error.to_string()))?;
    let internal = |error: IndexError| DocumentError::Internal(error.to_string());
    let head_index = RowIndex::build(head, dialect).map_err(internal)?;
    // No diagnostics here: the index job adds them (`start_index`).
    let (index, indexer) = RowIndex::start(dialect).map_err(internal)?;
    let whole_file = u64::try_from(head.len()).is_ok_and(|len| len == source.len());
    let head_rows = if whole_file {
        head_index.row_count()
    } else {
        // The last row may be cut off by the 64 KB limit (or end in a CR
        // whose LF is past it), so it is left to the real index.
        head_index.row_count().saturating_sub(1)
    };
    Ok(FirstPaint {
        detection,
        parser,
        head_index,
        head_rows,
        index,
        indexer,
    })
}

/// The first screen's rows, from the first 64 KB alone.
fn first_screen(
    head: &[u8],
    len: u64,
    generation: u64,
    paint: &FirstPaint,
    options: OpenOptions,
) -> FirstScreen {
    let rows = (0..paint.head_rows.min(options.first_screen_rows))
        .filter_map(|r| {
            let row = paint.parser.parse_row(&paint.head_index, r, head)?;
            Some(cells(
                &paint.parser,
                head,
                0,
                row.fields(),
                options.max_chars,
            ))
        })
        .collect();
    FirstScreen {
        generation,
        detection: paint.detection.clone(),
        rows,
        row_count: paint.head_rows,
        estimated_row_count: estimate_from_head(&paint.head_index, paint.head_rows, head, len),
        column_count: paint.head_index.field_count_mode().unwrap_or(0),
    }
}

/// Starts the index (P1) and the checks (P2) for a reading.
fn start_jobs(
    context: &Context<'_>,
    generation: u64,
    paint: FirstPaint,
    choices: Choices,
) -> Reading {
    let FirstPaint {
        detection,
        parser,
        head_index,
        head_rows,
        index,
        indexer,
    } = paint;
    let diagnostics = DiagnosticsSlot::default();
    let index_job = start_index(
        context,
        generation,
        &index,
        indexer,
        detection.encoding,
        &diagnostics,
    );
    let review_job = start_checks(context, &index_job, &detection);
    Reading {
        generation,
        choices,
        detection,
        parser,
        head_index,
        head_rows,
        index,
        diagnostics,
        index_job,
        review_job,
        cache: Mutex::new(RowCache::new(parser, DEFAULT_CACHE_ROWS)),
        cache_dropped: AtomicBool::new(false),
    }
}

/// P1: the row index, on its own thread, collecting the diagnostics of
/// text in `encoding` as it goes. It makes them first, on that thread, and
/// puts them in `diagnostics` for readers.
fn start_index(
    context: &Context<'_>,
    generation: u64,
    index: &Arc<RowIndex>,
    indexer: Indexer,
    encoding: Encoding,
    diagnostics: &DiagnosticsSlot,
) -> JobHandle<IndexSummary> {
    let source = Arc::clone(context.source);
    let progress = context.progress.cloned();
    let readers = Arc::clone(index);
    let slot = Arc::clone(diagnostics);
    context
        .scheduler
        .spawn(Priority::P1, Interval::Index, move |job| {
            // Detection always gives an encoding that fits the dialect, so
            // this doesn't fail; if it did, the index job would.
            let (indexer, diagnostics) = indexer.with_diagnostics(encoding)?;
            // Only this job sets the slot, and only here.
            let _ = slot.set(diagnostics);
            let report = |p: Progress| {
                // Records how long the chunk took; the index never pauses,
                // so this fails only if the job was cancelled.
                let checkpoint = job.checkpoint();
                if let Some(progress) = &progress {
                    progress(index_progress(generation, &readers, p));
                }
                checkpoint
            };
            index_file(&source, indexer, job, report)?;
            Ok(IndexSummary {
                rows: readers.row_count(),
                field_count_mode: readers.field_count_mode(),
                unterminated_quote: readers.unterminated_quote(),
            })
        })
}

/// Indexes the whole file: over the map if there is one, otherwise from
/// [`Source::stream`]'s chunks, which also copies a file on a removable
/// drive to the internal disk (ADR-0006). `report` is told after each
/// chunk, and its error (the job's checkpoint failing, because the job was
/// cancelled) stops the pass.
fn index_file(
    source: &Source,
    indexer: Indexer,
    job: &Job,
    mut report: impl FnMut(Progress) -> Result<(), JobError>,
) -> Result<(), JobError> {
    // The indexer collects diagnostics in the same pass, in both cases.
    if let Some(bytes) = source.as_slice() {
        // `run` checks the same cancel flag as the checkpoint before every
        // chunk, so a failed checkpoint stops it there.
        return Ok(indexer.run(bytes, job.cancel_flag(), |p| {
            let _ = report(p);
        })?);
    }
    let len =
        usize::try_from(source.len()).map_err(|_| IndexError::TooLarge { len: usize::MAX })?;
    let mut chunked = indexer.chunked(len)?;
    let mut failed: Option<JobError> = None;
    source.stream(job.cancel_flag(), |chunk| {
        if failed.is_some() {
            return;
        }
        // The stream's chunks are 1 MiB; indexing with diagnostics works in
        // smaller pieces (`DIAGNOSTICS_CHUNK_BYTES`), so that no stretch of
        // work between checkpoints is longer than over a mapped file. The
        // stream checks the cancel flag only between its chunks, so it is
        // checked before each piece too: a cancel takes effect within one
        // piece, as on a mapped file.
        for piece in chunk.bytes.chunks(DIAGNOSTICS_CHUNK_BYTES) {
            if job.is_cancelled() {
                failed = Some(JobError::Cancelled);
                return;
            }
            let reported = chunked
                .push(piece)
                .map_err(JobError::from)
                .and_then(&mut report);
            if let Err(error) = reported {
                failed = Some(error);
                return;
            }
        }
    })?;
    if let Some(error) = failed {
        return Err(error);
    }
    // The index is complete now, so a cancel that comes this late changes
    // nothing, as on a mapped file.
    let _ = report(chunked.finish()?);
    Ok(())
}

/// P2: the whole-file checks. On a mapped file they run alongside the
/// index; on a removable drive, once the index pass has copied the file and
/// mapped the copy. (Diagnostics need no P2 job: the index pass collects
/// all of them.)
fn start_checks(
    context: &Context<'_>,
    index_job: &JobHandle<IndexSummary>,
    detection: &Detection,
) -> JobHandle<Review> {
    let source = Arc::clone(context.source);
    let detection = detection.clone();
    let review = move |job: &Job| {
        let Some(bytes) = source.as_slice() else {
            return Err(unavailable(&source));
        };
        let review = review_with(bytes, &detection, || {
            job.checkpoint().map_err(|_| detect::Cancelled)
        })?;
        Ok(review)
    };
    if context.source.as_slice().is_some() {
        context
            .scheduler
            .spawn(Priority::P2, Interval::Review, review)
    } else {
        context
            .scheduler
            .spawn_after(index_job.control(), Priority::P2, Interval::Review, review)
    }
}

/// Why the whole file isn't there to review: its removable drive vanished
/// before the copy was complete, or it changed while it was read.
fn unavailable(source: &Source) -> JobError {
    if source.changed_on_disk() {
        JobError::Read(ReadErrorKind::ChangedOnDisk)
    } else if source.storage() == Storage::Disconnected {
        JobError::Read(ReadErrorKind::Disconnected)
    } else {
        JobError::Failed("the whole file isn't available to review".to_owned())
    }
}

/// Cells for `fields` of a parsed row. `bytes` are the file's bytes from
/// `base` on, and hold the row.
fn cells(
    parser: &RowParser,
    bytes: &[u8],
    base: usize,
    fields: &[FieldSpan],
    max_chars: usize,
) -> Vec<Cell> {
    fields
        .iter()
        .map(|field| {
            let (text, truncated) = parser.display_prefix_in(bytes, base, field, max_chars);
            Cell {
                text: text.into_owned(),
                truncated,
            }
        })
        .collect()
}

/// A progress report for the app.
fn index_progress(generation: u64, index: &RowIndex, progress: Progress) -> IndexProgress {
    let complete = index.status() == Status::Complete;
    IndexProgress {
        generation,
        rows: progress.rows,
        estimated_rows: index.estimated_row_count().unwrap_or(0).max(progress.rows),
        bytes_scanned: u64::try_from(progress.bytes_scanned).unwrap_or(u64::MAX),
        bytes_total: u64::try_from(progress.bytes_total).unwrap_or(u64::MAX),
        complete,
    }
}

/// The scrollbar's row count: the index's estimate once it has rows,
/// otherwise the first 64 KB's.
fn estimated_rows(reading: &Reading, head: &[u8], len: u64) -> usize {
    let index = &reading.index;
    if index.status() == Status::Complete {
        return index.row_count();
    }
    let from_index = index.estimated_row_count().unwrap_or(0);
    if index.row_count() >= reading.head_rows && from_index > 0 {
        from_index
    } else {
        estimate_from_head(&reading.head_index, reading.head_rows, head, len)
    }
}

/// The file's size over the average length of the whole rows in the first
/// 64 KB (DESIGN §3.10 rule 5), and exact if the file fits in them.
fn estimate_from_head(head_index: &RowIndex, head_rows: usize, head: &[u8], len: u64) -> usize {
    let whole_file = u64::try_from(head.len()).is_ok_and(|n| n == len);
    let Some(extent) = head_index.rows_extent(0..head_rows) else {
        return head_rows;
    };
    if whole_file || extent.is_empty() {
        return head_rows;
    }
    let rows = u128::try_from(head_rows).unwrap_or(u128::MAX);
    let spanned = u128::try_from(extent.len()).unwrap_or(u128::MAX);
    let total = u128::from(len.saturating_sub(u64::try_from(extent.start).unwrap_or(0)));
    let estimate = (rows * total).div_ceil(spanned);
    usize::try_from(estimate)
        .unwrap_or(usize::MAX)
        .max(head_rows)
}
