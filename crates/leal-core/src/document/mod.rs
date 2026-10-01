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
//!   [`Document::diagnostics`] gives the report of the rows indexed so far,
//!   and [`Document::row_has_diagnostic`] and its neighbours mark every
//!   affected row.
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

#[cfg(test)]
mod tests;

use std::borrow::Cow;
use std::fmt;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use crate::detect::{
    self, ChoiceError, Choices, Detection, FIRST_PAINT_BYTES, Hints, Review, detect, review_with,
};
use crate::diagnostics::{Diagnostics, Report};
use crate::dialect::QUOTE;
use crate::index::{
    DIAGNOSTICS_CHUNK_BYTES, IndexDialect, IndexError, Indexer, MAX_FILE_BYTES, Progress, RowIndex,
    Status,
};
use crate::rows::{DEFAULT_CACHE_ROWS, ParsedRow, RowCache, RowParser};
use crate::schedule::{Interval, IntervalGuard, Job, JobError, JobHandle, Priority, Scheduler};
use crate::source::{
    OpenError, ReadError, ReadErrorKind, Source, Storage, TempFolders, VolumeInfo,
};

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
    reading: RwLock<Arc<Reading>>,
}

/// One reading of the file: a detection, and the index and jobs built
/// from it. [`Document::reinterpret`] replaces it.
struct Reading {
    generation: u64,
    detection: Detection,
    parser: RowParser,
    /// The first 64 KB on their own, for rows before the index has them.
    head_index: RowIndex,
    /// How many of `head_index`'s rows are whole: all but the last, which
    /// may be cut, unless the file fits in 64 KB.
    head_rows: usize,
    index: Arc<RowIndex>,
    /// What the index pass finds wrong with the file, so far.
    diagnostics: Arc<Diagnostics>,
    index_job: JobHandle<IndexSummary>,
    review_job: JobHandle<Review>,
    cache: Mutex<RowCache>,
}

/// First paint's result (P0), before any job starts.
struct FirstPaint {
    detection: Detection,
    parser: RowParser,
    head_index: RowIndex,
    head_rows: usize,
    /// The real index, empty, and the indexer that will fill it and
    /// collect the diagnostics.
    index: Arc<RowIndex>,
    diagnostics: Arc<Diagnostics>,
    indexer: Indexer,
}

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
        let reading = start_jobs(&context, 0, paint);
        let document = Document {
            scheduler: scheduler.clone(),
            head,
            progress,
            generations: AtomicU64::new(1),
            reinterpreting: Mutex::new(()),
            reading: RwLock::new(Arc::new(reading)),
            source,
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
        let reading = start_jobs(&context, generation, paint);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(reading);
        Ok(screen)
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
        let reading = self.current();
        let indexed = reading.index.row_count();
        // Both indexes cover a prefix of the file's rows; use the longer.
        let (index, available) = if indexed >= reading.head_rows {
            (&*reading.index, indexed)
        } else {
            (&reading.head_index, reading.head_rows)
        };
        let rows = rows.start..rows.end.min(available);
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok(Vec::new());
        };
        // Rows in the first 64 KB come from the copy kept in memory.
        let bytes: Cow<'_, [u8]> = match self.head.get(extent.clone()) {
            Some(bytes) => Cow::Borrowed(bytes),
            None => self.source.read_range(extent.clone())?,
        };
        let base = extent.start;
        let mut cache = reading.cache.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(rows
            .filter_map(|r| {
                let row = cache.row_in(index, r, &bytes, base)?;
                Some(cells(&reading.parser, &bytes, base, &row, max_chars))
            })
            .collect())
    }

    /// The number of rows that can be read now: the index's so far, or the
    /// first 64 KB's if the index hasn't got that far. Once indexing is
    /// complete, the file's row count.
    #[must_use]
    pub fn row_count(&self) -> usize {
        let reading = self.current();
        reading.index.row_count().max(reading.head_rows)
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
            rows: index.row_count().max(reading.head_rows),
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
        self.current().diagnostics.report()
    }

    /// True if row `row` has a warning or an error, for its gutter marker
    /// (every such row, not only the report's first locations). False for a
    /// row not indexed yet.
    #[must_use]
    pub fn row_has_diagnostic(&self, row: usize) -> bool {
        self.current().diagnostics.row_has_diagnostic(row)
    }

    /// The first row at or after `from` with a warning or an error, for
    /// **Next**.
    #[must_use]
    pub fn next_row_with_diagnostic(&self, from: usize) -> Option<usize> {
        self.current().diagnostics.next_row_with_diagnostic(from)
    }

    /// The last row before `to` with a warning or an error, for
    /// **Previous**.
    #[must_use]
    pub fn previous_row_with_diagnostic(&self, to: usize) -> Option<usize> {
        self.current().diagnostics.previous_row_with_diagnostic(to)
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
    let (index, diagnostics, indexer) =
        RowIndex::start_with_diagnostics(dialect, detection.encoding).map_err(internal)?;
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
        diagnostics,
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
            Some(cells(&paint.parser, head, 0, &row, options.max_chars))
        })
        .collect();
    FirstScreen {
        generation,
        detection: paint.detection.clone(),
        rows,
        row_count: paint.head_rows,
        estimated_row_count: estimate_from_head(&paint.head_index, paint.head_rows, head, len),
    }
}

/// Starts the index (P1) and the checks (P2) for a reading.
fn start_jobs(context: &Context<'_>, generation: u64, paint: FirstPaint) -> Reading {
    let FirstPaint {
        detection,
        parser,
        head_index,
        head_rows,
        index,
        diagnostics,
        indexer,
    } = paint;
    let index_job = start_index(context, generation, &index, indexer);
    let review_job = start_checks(context, &index_job, &detection);
    Reading {
        generation,
        detection,
        parser,
        head_index,
        head_rows,
        index,
        diagnostics,
        index_job,
        review_job,
        cache: Mutex::new(RowCache::new(parser, DEFAULT_CACHE_ROWS)),
    }
}

/// P1: the row index, on its own thread.
fn start_index(
    context: &Context<'_>,
    generation: u64,
    index: &Arc<RowIndex>,
    indexer: Indexer,
) -> JobHandle<IndexSummary> {
    let source = Arc::clone(context.source);
    let progress = context.progress.cloned();
    let readers = Arc::clone(index);
    context
        .scheduler
        .spawn(Priority::P1, Interval::Index, move |job| {
            let report = |p: Progress| {
                // Records how long the chunk took; the index never pauses.
                let _ = job.checkpoint();
                if let Some(progress) = &progress {
                    progress(index_progress(generation, &readers, p));
                }
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
/// drive to the internal disk (ADR-0006).
fn index_file(
    source: &Source,
    indexer: Indexer,
    job: &Job,
    mut report: impl FnMut(Progress),
) -> Result<(), JobError> {
    // The indexer collects diagnostics in the same pass, in both cases.
    if let Some(bytes) = source.as_slice() {
        return Ok(indexer.run(bytes, job.cancel_flag(), report)?);
    }
    let len =
        usize::try_from(source.len()).map_err(|_| IndexError::TooLarge { len: usize::MAX })?;
    let mut chunked = indexer.chunked(len)?;
    let mut failed = None;
    source.stream(job.cancel_flag(), |chunk| {
        if failed.is_some() {
            return;
        }
        // The stream's chunks are 1 MiB; indexing with diagnostics works in
        // smaller pieces (`DIAGNOSTICS_CHUNK_BYTES`), so that no stretch of
        // work between checkpoints is longer than over a mapped file.
        for piece in chunk.bytes.chunks(DIAGNOSTICS_CHUNK_BYTES) {
            match chunked.push(piece) {
                Ok(progress) => report(progress),
                Err(error) => {
                    failed = Some(error);
                    return;
                }
            }
        }
    })?;
    if let Some(error) = failed {
        return Err(error.into());
    }
    report(chunked.finish()?);
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

/// A parsed row's cells. `bytes` are the file's bytes from `base` on, and
/// hold the row.
fn cells(
    parser: &RowParser,
    bytes: &[u8],
    base: usize,
    row: &ParsedRow,
    max_chars: usize,
) -> Vec<Cell> {
    row.fields()
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
