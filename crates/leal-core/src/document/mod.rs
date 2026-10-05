//! An open document: opening a file the way DESIGN §3.10 asks, and reading
//! its rows while the background work runs.
//!
//! [`Document::open`] does the first-paint work (P0) on the calling thread
//! and returns the [`FirstScreen`] without waiting for anything else:
//!
//! 1. open the [`Source`] (clone and map, or, on a removable drive or a
//!    network share, ordinary reads);
//! 2. read the first 64 KB (one read, [`Source::read_head`]) and
//!    [`detect()`] the encoding and dialect from it;
//! 3. index those 64 KB on their own and parse the first screen of rows
//!    from them. Their offsets are the same as in the whole file, so they
//!    serve rows until the real index has them.
//!
//! Only then does it start the background jobs, through the [`Scheduler`]:
//!
//! - **P1, the row index**, on its own thread. On an internal volume it
//!   scans the mapped file ([`Indexer::run`]). On a removable drive or a
//!   network share it scans [`Source::stream`]'s chunks
//!   ([`Indexer::chunked`]), the same pass that copies the file to the
//!   internal disk (ADR-0006, ADR-0009), so the file is read once.
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
//! mapped). Every row the index has is already in the internal copy (the
//! stream copies each chunk before the index sees it), so on a network
//! share rows are never read from the share itself: a row the index
//! hasn't reached isn't returned, and the grid shows it as loading until
//! the index pass, in the background, brings it (ADR-0009).
//!
//! **Opening a file on a network share** must be off the main thread
//! ([`Document::open`]): first paint reads the share. [`Document::reinterpret`] reads the file again with another
//! delimiter, header or encoding (**Treat as**, **Reopen with encoding…**)
//! without reopening it: it cancels the old jobs and starts new ones.
//!
//! **Edits** (task 2.1, [`crate::edit`]). [`Document::set_cell`] edits a
//! cell and returns the command, which the app's undo manager keeps;
//! [`Document::apply`] undoes and redoes, and [`Document::replay`]
//! recovers a failed document's edits into a fresh one. The edits live in
//! an overlay shared by the readings that split the file the same way.
//! Every reader (the grid, the inspector, Copy, Find, the diagnostics marks
//! and number detection) sees a row through a `RowView`, the row's fields
//! with the overlay on top, so they all show the cells as they read now
//! (ADR-0008 decision 2).
//!
//! [`Indexer::run`]: crate::index::Indexer::run
//! [`Indexer::chunked`]: crate::index::Indexer::chunked
//! [`Priority::P3`]: crate::schedule::Priority::P3

mod columns;
mod editing;
mod pasting;
mod remember;
mod saving;
mod search;
mod structural;
#[cfg(test)]
mod tests;
mod values;
mod view;

pub use pasting::Pasting;
#[cfg(any(test, feature = "test-hooks"))]
#[doc(hidden)]
pub use saving::{AT_SWAP, BEFORE_ADOPT, ChunkHook};
pub use saving::{SAVE_CHUNK_BYTES, SaveJob};

/// A document read again after its removable drive came back
/// ([`Document::check_original_restarting`]): the generation it was, and
/// the one it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restarted {
    /// The reading's generation before.
    pub from: u64,
    /// Its generation now.
    pub to: u64,
}
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
use crate::diagnostics::{RowCode, RowMarks};
use crate::dialect::{Encoding, QUOTE};
use crate::edit::{
    CellId, Columns, EditStore, InsertedRow, Kinds, Overlay, OverlayRow, Own as EditOwn, RowEdits,
    RowId, Segment, Slot, TABLE,
};
use crate::index::{
    DIAGNOSTICS_CHUNK_BYTES, IndexDialect, IndexError, Indexer, MAX_FILE_BYTES, Progress, RowIndex,
    Status,
};
use crate::rows::{
    DEFAULT_CACHE_ROWS, FieldSpan, NUMBER_MAX_CHARS, NumericColumns, ParsedRow, RowCache, RowParser,
};
use crate::schedule::{
    Interval, IntervalGuard, Job, JobError, JobHandle, Priority, Scheduler, ThreadClass,
};
use crate::source::{
    OpenError, Original, OriginalState, OriginalStatus, ReadError, ReadErrorKind, Source, Storage,
    TempFolders, VolumeInfo,
};
pub(crate) use view::{RowView, ViewCell};

/// Where an occurrence of a diagnostic is, for the details popover's
/// **Previous** and **Next** (task 1.7): the cell to select.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// The 0-based logical row (the header row, if any, is row 0).
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
    /// The columns of the window's cells that hold an edit (a value that
    /// reads differently from the file's), in order: for the grid's
    /// edited-cell marks (task 2.5.2, mockup 05a). A new row's own values
    /// and a column insert's aren't edits. Empty for a row with no edits.
    pub edited: Vec<usize>,
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
    /// vanished straight after opening, or a network share that stopped
    /// answering).
    Read(ReadError),
    /// The user's encoding doesn't fit the file's BOM.
    Choice(ChoiceError),
    /// The file is larger than Leal reads ([`MAX_FILE_BYTES`], DESIGN §1).
    TooLarge {
        /// The file's length.
        len: u64,
    },
    /// The file has unsaved edits, so it can't be read with another
    /// delimiter or encoding (ADR-0008 decision 4): edits are tied to how
    /// the file was split into cells. The header row can still change.
    UnsavedEdits,
    /// A save is running (task 2.2): the file is read again, the new way,
    /// only once it has finished, since the save replaces the reading.
    Saving,
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
            DocumentError::UnsavedEdits => f.write_str(
                "the file can't be read with another delimiter or encoding while it has unsaved edits",
            ),
            DocumentError::Saving => f.write_str("the file is being saved"),
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
    scheduler: Scheduler,
    progress: Option<ProgressCallback>,
    /// The next reading's generation.
    generations: AtomicU64,
    /// Held while the document changes (`reinterpret`, `restart` and every
    /// edit), so changes happen one at a time: two reinterprets can't leave
    /// a reading whose jobs nobody cancels, and an edit can't be checked
    /// against one reading and land in the next.
    writer: Mutex<()>,
    /// Counts kind searches (`next_with_kind`): a search stops when a
    /// newer one starts.
    searches: AtomicU64,
    /// A save is running or queued (task 2.2): saves take turns, and the
    /// file isn't read again meanwhile.
    saving: AtomicBool,
    /// A check of the user's file found its drive back while a save ran,
    /// and left reconnecting to it until the save ends
    /// ([`check_original`](Self::check_original)).
    recheck: AtomicBool,
    /// The current reading. Shared with the watching thread
    /// ([`watch_original`](Self::watch_original)), which notes what it sees
    /// on the reading's source, whichever reading is current then: a save
    /// replaces the source (task 2.2).
    reading: Arc<RwLock<Arc<Reading>>>,
    /// The user's file, watched for changes made elsewhere (task 1.9).
    original: Original,
}

/// One reading of the file: a detection, and the index and jobs built
/// from it. [`Document::reinterpret`] replaces it.
struct Reading {
    /// The file's bytes: the snapshot taken when it was opened, or when
    /// Leal last saved it (task 2.2).
    source: Arc<Source>,
    /// The first 64 KB of `source`, read once at first paint and kept:
    /// re-reading the file with other choices starts from them, and rows in
    /// them can be read even if a removable drive vanishes before they were
    /// copied.
    head: Arc<[u8]>,
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
    /// The document's edits (task 2.1). Shared with the readings before and
    /// after this one while they split the file the same way, so the header
    /// toggle and a drive reconnecting keep them (ADR-0008 decision 4).
    edits: Arc<EditStore>,
    /// Every row's field count, from the save that wrote the file (task
    /// 2.4c), so column operations needn't wait for the marks of its index
    /// pass. `None` for a file opened or read again.
    counts: Option<RowMarks>,
    /// A save made this reading, of the file it wrote (task 2.5.3b): its
    /// review may add the interpretation attribute
    /// ([`Document::remember_reviewed_interpretation`]).
    saved: bool,
    /// Its review's delimiter and header were written to the file's
    /// interpretation attribute (task 2.5.3b review): from then on they are
    /// the attribute's choice, which every later save records itself
    /// (ADR-0008 decision 8).
    remembered: AtomicBool,
    /// The longest row's length, worked out for the edits as they were
    /// when last asked: the Edit menu's column items ask each time it
    /// opens ([`Document::can_insert_column`]), and finding it looks at
    /// every row.
    widest: Mutex<Option<columns::Widest>>,
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
    head: &'a Arc<[u8]>,
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
    /// It reads the file's volume, so call it off the main thread if the
    /// file may be on a network share: a share that stops answering can
    /// block it, and first paint's read of a share retries network errors
    /// for up to about 3 s (ADR-0009). A debug build panics if a share is
    /// read on the main thread.
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
        // slice of the map; the copy costs a few microseconds. On a network
        // share it is the one read of the share outside the index pass.
        let head: Arc<[u8]> = Arc::from(&*source.read_head(FIRST_PAINT_BYTES)?);
        let source = Arc::new(source);
        let paint = read_first_paint(&source, &head, options.choices)?;
        let screen = first_screen(&head, len, 0, &paint, options, &Overlay::default());
        // First paint is done: everything after this is background work.
        drop(first_paint);
        let context = Context {
            source: &source,
            head: &head,
            scheduler,
            progress: progress.as_ref(),
        };
        let reading = start_jobs(&context, 0, paint, options.choices, Arc::default());
        // One look at the user's file now (a few system calls), so a change
        // between opening it and here isn't missed; watching it starts when
        // the app asks (`watch_original`).
        let original = Original::new(source.path(), *source.identity());
        let document = Document {
            scheduler: scheduler.clone(),
            progress,
            generations: AtomicU64::new(1),
            writer: Mutex::new(()),
            searches: AtomicU64::new(0),
            saving: AtomicBool::new(false),
            recheck: AtomicBool::new(false),
            reading: Arc::new(RwLock::new(Arc::new(reading))),
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
    /// file's BOM, [`DocumentError::UnsavedEdits`] if the file has edits
    /// and would be split into cells another way (another delimiter or
    /// encoding: ADR-0008 decision 4; the header row can change, and the
    /// edits are kept), and [`DocumentError::Read`] with
    /// [`ReadErrorKind::ChangedOnDisk`] once the file changed while it was
    /// read ([`changed_on_disk`](Self::changed_on_disk)). The document is
    /// then unchanged.
    ///
    /// After such a change the first 64 KB kept in memory may be the old
    /// version, so detection and the first screen can't come from them, and
    /// the file can't be read again either: it is never copied whole
    /// ([`Source::stream`] refuses), so a new index would have no rows. The
    /// current reading still serves the rows its index checked, and the
    /// app offers Reload (p1-review fid-2).
    pub fn reinterpret(
        &self,
        choices: Choices,
        first_screen_rows: usize,
        max_chars: usize,
    ) -> Result<FirstScreen, DocumentError> {
        // Refused before waiting for the writer lock as well as after.
        self.refuse_while_saving()?;
        let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        self.refuse_while_saving()?;
        let first_paint = self.scheduler.interval(Interval::FirstPaint);
        let old = self.current();
        let paint = read_first_paint(&old.source, &old.head, choices)?;
        // Checked after detection, so a change noticed meanwhile counts.
        old.refuse_once_stale()?;
        let edits = edits_after(&old, &paint.detection)?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let options = OpenOptions {
            choices,
            first_screen_rows,
            max_chars,
        };
        let screen = first_screen(
            &old.head,
            old.source.len(),
            generation,
            &paint,
            options,
            &edits.overlay(),
        );
        drop(first_paint);
        // Stop the old jobs first: a removable drive's stream is one pass
        // at a time, so the new index waits for the old one to stop.
        old.cancel();
        let context = Context {
            source: &old.source,
            head: &old.head,
            scheduler: &self.scheduler,
            progress: self.progress.as_ref(),
        };
        let reading = start_jobs(&context, generation, paint, choices, edits);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(reading);
        Ok(screen)
    }

    /// Reconnects `source` (the current reading's) to the user's file at
    /// `path`, its removable drive back, and reads the file again the way
    /// the current reading does, with a new index and review and a new
    /// generation, so the index pass carries on copying it. The first
    /// screen is the same as before. While a save runs, nothing is done
    /// and the save checks again when it ends ([`recheck`](Self::recheck)):
    /// it may replace the reading. `None` if nothing was restarted.
    fn reconnect_and_restart(&self, source: &Arc<Source>, path: &Path) -> Option<Restarted> {
        // `recheck` is set before `saving` is looked at, and a save clears
        // `saving` before it looks at `recheck` (both `SeqCst`): if this
        // sees a save running, that save sees `recheck` when it ends.
        self.recheck.store(true, Ordering::SeqCst);
        if self.saving.load(Ordering::SeqCst) {
            return None;
        }
        // A save takes its turn before it takes the writer lock for its
        // snapshot, so under the lock, a save not seen yet starts on the
        // reading made here.
        let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        if self.saving.load(Ordering::SeqCst) {
            return None;
        }
        // Done here, not left for a save.
        self.recheck.store(false, Ordering::SeqCst);
        let old = self.current();
        if !Arc::ptr_eq(&old.source, source) || !source.reconnect(path) {
            return None;
        }
        // Detection already succeeded with these choices on these bytes, so
        // this can't fail; if it did, the source stays reconnected, read
        // the old way.
        let paint = read_first_paint(&old.source, &old.head, old.choices).ok()?;
        old.refuse_once_stale().ok()?;
        // The same choices on the same bytes split the file the same way,
        // so the edits carry on (ADR-0008 decision 4).
        let edits = edits_after(&old, &paint.detection).ok()?;
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        old.cancel();
        let context = Context {
            source: &old.source,
            head: &old.head,
            scheduler: &self.scheduler,
            progress: self.progress.as_ref(),
        };
        let reading = start_jobs(&context, generation, paint, old.choices, edits);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(reading);
        Some(Restarted {
            from: old.generation,
            to: generation,
        })
    }

    /// [`reinterpret`](Self::reinterpret)'s and `restart`'s refusal while
    /// a save runs: the save replaces the reading when it ends.
    fn refuse_while_saving(&self) -> Result<(), DocumentError> {
        if self.saving.load(Ordering::Acquire) {
            Err(DocumentError::Saving)
        } else {
            Ok(())
        }
    }

    /// Rows `rows` (as many of them as can be read now), each as its
    /// cells, with at most `max_chars` characters of each. Fast enough for
    /// the main thread: it reads the rows' bytes once, as one range, and
    /// keeps recently parsed rows in a small cache. Cells are as they read
    /// now: an edited row has its edited values, and as many cells as it
    /// has now (a hatched cell edited past its end lengthens it).
    ///
    /// Rows the index hasn't reached yet, beyond those in the first 64 KB,
    /// aren't returned: the result is shorter, or empty.
    ///
    /// # Errors
    ///
    /// A [`ReadError`] if the bytes can't be read. That happens only for a
    /// file on a removable drive or a network share whose copy isn't
    /// complete: for example, rows that weren't copied before the drive
    /// vanished ([`ReadErrorKind::Disconnected`]) or the file was deleted on
    /// its share ([`ReadErrorKind::Deleted`]).
    pub fn rows(&self, rows: Range<usize>, max_chars: usize) -> Result<Vec<Vec<Cell>>, ReadError> {
        self.read_rows(rows, |view| row_cells(&view, max_chars))
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
        self.read_rows(rows, |view| {
            let count = view.len();
            // Clamped to the row: a window past its end is empty. (`clamp`
            // panics if its minimum is past its maximum, so the start is
            // clamped first.)
            let start = columns.start.min(count);
            let window = start..columns.end.clamp(start, count);
            let (cells, edited) = if let Some(parsed) = view.plain() {
                let fields = parsed.fields().get(window).unwrap_or_default();
                let cells = cells(view.parser(), view.bytes(), view.base(), fields, max_chars);
                (cells, Vec::new())
            } else {
                edited_cells(&view, window, max_chars)
            };
            RowCells {
                field_count: count,
                cells,
                edited,
            }
        })
    }

    /// The grid's column count: the most common field count among the
    /// rows read so far (ADR-0003 decision 4), from the first 64 KB until
    /// the index has passed them. It can change while indexing and is
    /// final once the index is complete. 0 for an empty file. Once columns
    /// are inserted or deleted (task 2.4b), the length a row of that many
    /// fields has after them.
    #[must_use]
    pub fn column_count(&self) -> usize {
        let reading = self.current();
        if reading.edits.map_len() == Some(0) {
            // Every row deleted.
            return 0;
        }
        let mode = Self::rows_index(&reading).0.field_count_mode();
        let columns = reading.edits.columns();
        mode.map_or(0, |mode| columns.fold_fields(mode))
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
        self.read_rows(first..first.saturating_add(sample), |view| {
            for column in 0..view.len() {
                if let Some((text, truncated)) = view.prefix(column, NUMBER_MAX_CHARS) {
                    columns.add(column, &text, truncated);
                }
            }
        })?;
        Ok(columns.result())
    }

    /// Reads rows `rows` (as many as can be read now) and hands each to
    /// `each`, as it reads now: its parsed fields, with the edit overlay on
    /// top ([`RowView`]). One read of the file per call.
    fn read_rows<T>(
        &self,
        rows: Range<usize>,
        each: impl FnMut(RowView<'_>) -> T,
    ) -> Result<Vec<T>, ReadError> {
        Self::read_rows_of(&self.current(), rows, each)
    }

    /// [`read_rows`](Self::read_rows), in a given reading. `rows` are
    /// logical rows: they are read a segment of the piece list at a time,
    /// stretches of the file's rows as one read each, and inserted rows
    /// from the edits.
    fn read_rows_of<T>(
        reading: &Reading,
        rows: Range<usize>,
        each: impl FnMut(RowView<'_>) -> T,
    ) -> Result<Vec<T>, ReadError> {
        // One look at the edits for the whole call: a reference count, and
        // for each row a look-up that finds nothing when there are no edits.
        let overlay = reading.edits.overlay();
        Self::read_rows_with(reading, &overlay, rows, each)
    }

    /// [`read_rows_of`](Self::read_rows_of), with `overlay`'s edits (a
    /// snapshot a command keeps, task 2.4b).
    fn read_rows_with<T>(
        reading: &Reading,
        overlay: &Overlay,
        rows: Range<usize>,
        mut each: impl FnMut(RowView<'_>) -> T,
    ) -> Result<Vec<T>, ReadError> {
        let map = overlay.map();
        if map.is_identity() {
            return Self::read_physical(reading, overlay, rows, &mut each);
        }
        let available = map.rows_within(Self::rows_index(reading).1);
        let mut out = Vec::new();
        for segment in map.segments(rows.start..rows.end.min(available)) {
            match segment {
                Segment::Original(range) => out.extend(Self::read_physical(
                    reading,
                    overlay,
                    to_usize(range.start)..to_usize(range.end),
                    &mut each,
                )?),
                Segment::Inserted(range) => {
                    for n in range {
                        if let Some((row, cells)) = inserted_row(overlay, n) {
                            out.push(each(RowView::inserted(&reading.parser, row, cells)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// [`read_rows_with`](Self::read_rows_with), parsed without the row
    /// cache, as [`each_physical`](Self::each_physical) parses them: for a
    /// pass over many rows (an undo or redo by value, a column's cells, a
    /// duplicate), which through the cache paid an eviction, a look at
    /// every cached row, for each row read, and only evicted the grid's
    /// rows (phase 2 gate, `docs/tasks/2.G-a.md`).
    fn read_rows_uncached<T>(
        reading: &Reading,
        overlay: &Overlay,
        rows: Range<usize>,
        mut each: impl FnMut(RowView<'_>) -> T,
    ) -> Result<Vec<T>, ReadError> {
        let mut out = Vec::new();
        let map = overlay.map();
        if map.is_identity() {
            Self::each_physical(reading, overlay, rows, &mut |view| out.push(each(view)))?;
            return Ok(out);
        }
        let available = map.rows_within(Self::rows_index(reading).1);
        for segment in map.segments(rows.start..rows.end.min(available)) {
            match segment {
                Segment::Original(range) => Self::each_physical(
                    reading,
                    overlay,
                    to_usize(range.start)..to_usize(range.end),
                    &mut |view| out.push(each(view)),
                )?,
                Segment::Inserted(range) => {
                    for n in range {
                        if let Some((row, cells)) = inserted_row(overlay, n) {
                            out.push(each(RowView::inserted(&reading.parser, row, cells)));
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    /// Physical rows `rows` (as many as can be read now), each to `each`
    /// with `overlay`'s edits on top. One read of the file.
    fn read_physical<T>(
        reading: &Reading,
        overlay: &Overlay,
        rows: Range<usize>,
        each: &mut impl FnMut(RowView<'_>) -> T,
    ) -> Result<Vec<T>, ReadError> {
        // One look, so the rows and their bytes agree on whether the first
        // 64 KB can be trusted.
        let stale = reading.head_is_stale();
        let (index, available) = reading.rows_from(&reading.index, stale);
        let rows = rows.start..rows.end.min(available);
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok(Vec::new());
        };
        let bytes = reading.bytes_of(extent.clone(), stale)?;
        let base = extent.start;
        // The cache is locked only to find (or parse) the rows, never while
        // `each` runs: `each` may decode whole values, megabytes long for
        // the inspector (`cell_value`), and the main thread's `rows` needs
        // the lock meanwhile (p1-review conc-2).
        let parsed: Vec<(usize, Arc<ParsedRow>)> = {
            let mut cache = reading.cache.lock().unwrap_or_else(PoisonError::into_inner);
            if stale && !reading.cache_dropped.swap(true, Ordering::AcqRel) {
                // Rows parsed before the change was noticed may be from
                // either version of the file.
                cache.clear();
            }
            rows.filter_map(|r| Some((r, cache.row_in(index, r, &bytes, base)?)))
                .collect()
        };
        Ok(parsed
            .iter()
            .map(|(r, row)| {
                each(RowView::new(
                    &reading.parser,
                    &bytes,
                    base,
                    row,
                    overlay.physical(*r),
                ))
            })
            .collect())
    }

    /// Physical rows `rows` (as many as can be read now), each to `each`
    /// with `overlay`'s edits on top, as [`read_physical`] gives them, but
    /// parsed without the row cache: for a pass over every row (a save's,
    /// task 2.4c), which would only evict the grid's rows, and hold the
    /// cache's lock from the main thread a batch at a time.
    ///
    /// [`read_physical`]: Self::read_physical
    fn each_physical(
        reading: &Reading,
        overlay: &Overlay,
        rows: Range<usize>,
        each: &mut dyn FnMut(RowView<'_>),
    ) -> Result<(), ReadError> {
        let stale = reading.head_is_stale();
        let (index, available) = reading.rows_from(&reading.index, stale);
        let rows = rows.start..rows.end.min(available);
        let Some(extent) = index.rows_extent(rows.clone()) else {
            return Ok(());
        };
        let bytes = reading.bytes_of(extent.clone(), stale)?;
        let base = extent.start;
        for row in rows {
            if let Some(parsed) = reading.parser.parse_row_in(index, row, &bytes, base) {
                each(RowView::new(
                    &reading.parser,
                    &bytes,
                    base,
                    &parsed,
                    overlay.physical(row),
                ));
            }
        }
        Ok(())
    }

    /// The number of rows that can be read now: the index's so far, or the
    /// first 64 KB's if the index hasn't got that far. Once indexing is
    /// complete, the file's row count. After the file changed while it was
    /// read ([`changed_on_disk`](Self::changed_on_disk)), only the index's.
    #[must_use]
    pub fn row_count(&self) -> usize {
        Self::logical_rows(&self.current())
    }

    /// The logical rows that can be read now: the rows the index has, with
    /// rows inserted and deleted (task 2.4a).
    fn logical_rows(reading: &Reading) -> usize {
        reading.edits.rows_within(Self::rows_index(reading).1)
    }

    /// The index rows are read from, and how many rows it has
    /// ([`Reading::rows_from`] the reading's index).
    fn rows_index(reading: &Reading) -> (&RowIndex, usize) {
        reading.rows_from(&reading.index, reading.head_is_stale())
    }

    /// The row count to size the scrollbar with while indexing (DESIGN
    /// §3.10 rule 5): exact once indexing is complete.
    #[must_use]
    pub fn estimated_row_count(&self) -> usize {
        let reading = self.current();
        let estimate = estimated_rows(&reading, &reading.head, reading.source.len());
        reading.edits.shift(estimate)
    }

    /// Where indexing has got to.
    #[must_use]
    pub fn progress(&self) -> IndexProgress {
        let reading = self.current();
        let index = &reading.index;
        IndexProgress {
            generation: reading.generation,
            rows: Self::logical_rows(&reading),
            estimated_rows: reading.edits.shift(estimated_rows(
                &reading,
                &reading.head,
                reading.source.len(),
            )),
            bytes_scanned: u64::try_from(index.bytes_scanned()).unwrap_or(u64::MAX),
            bytes_total: reading.source.len(),
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

    /// The file's bytes: the snapshot the current reading reads, which a
    /// save replaces with one of the file it wrote (task 2.2).
    #[must_use]
    pub fn source(&self) -> Arc<Source> {
        Arc::clone(&self.current().source)
    }

    /// Where the file's bytes are held ([`Source::storage`]): for a file on
    /// a removable drive, this changes from [`Storage::Reading`] to
    /// [`Storage::Copy`] when the index pass has copied it.
    #[must_use]
    pub fn storage(&self) -> Storage {
        self.current().source.storage()
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
    /// row not indexed yet. An edited row is marked as it reads now: its
    /// edited cells are checked on their new values, and its field count is
    /// the one it has now (ADR-0008 decision 2). An inserted row is marked
    /// from its cells: ragged, or holding a NUL.
    #[must_use]
    pub fn row_has_diagnostic(&self, row: usize) -> bool {
        let reading = self.current();
        let Some(diagnostics) = reading.diagnostics.get() else {
            return false;
        };
        let overlay = reading.edits.overlay();
        match overlay.map().slot(row) {
            None => false,
            Some(Slot::Original(row)) => {
                let row = to_usize(row);
                if let Some(edits) = overlay.row(row) {
                    let (marked, mode) = diagnostics.marked_rows_and_mode();
                    let flags = Self::edited_flags(&reading, &overlay, row, edits, mode);
                    return row < marked && flags.marked;
                }
                if !overlay.columns().is_empty() {
                    let marks = FoldedMarks::new(overlay.columns(), diagnostics);
                    return diagnostics
                        .code_of(row)
                        .is_some_and(|code| marks.row_is(&reading, &overlay, row, code, None));
                }
                diagnostics.row_has_diagnostic(row)
            }
            Some(Slot::Inserted(n)) => {
                let mode = folded(&overlay, diagnostics.marked_rows_and_mode().1);
                inserted_flags(&reading, &overlay, n, mode).marked
            }
        }
    }

    /// The first row at or after `from` with a warning or an error, for
    /// **Next**. Edited rows are marked as they read now, as for
    /// [`row_has_diagnostic`](Self::row_has_diagnostic); deleted rows are
    /// passed over.
    #[must_use]
    pub fn next_row_with_diagnostic(&self, from: usize) -> Option<usize> {
        let reading = self.current();
        let diagnostics = reading.diagnostics.get()?;
        let overlay = reading.edits.overlay();
        if overlay.is_empty() {
            return diagnostics.next_row_with_diagnostic(from);
        }
        let map = overlay.map();
        let Some(len) = map.len() else {
            return Self::next_marked(&reading, diagnostics, &overlay, from..usize::MAX);
        };
        if from >= len {
            return None;
        }
        // One search through the file's own rows from where `from` is among
        // them, passing over deleted ones, then any inserted row between
        // `from` and what it found.
        let mode = folded(&overlay, diagnostics.marked_rows_and_mode().1);
        let rows = map.physical_rows().unwrap_or(0);
        let mut physical = map.physical_at_or_after(from);
        let found = loop {
            let Some(row) =
                Self::next_marked(&reading, diagnostics, &overlay, physical..usize::MAX)
            else {
                break None;
            };
            let row = u32::try_from(row).unwrap_or(u32::MAX);
            match map.logical_of(row) {
                Ok(logical) => break Some(logical),
                Err(_) if row < rows => physical = to_usize(map.next_live(row + 1)),
                Err(_) => break None,
            }
        };
        let end = found.unwrap_or(len);
        let mut logical = from;
        for segment in map.segments(from..end) {
            if let Segment::Inserted(range) = &segment {
                for (k, n) in range.clone().enumerate() {
                    if inserted_flags(&reading, &overlay, n, mode).marked {
                        return Some(logical + k);
                    }
                }
            }
            logical += segment.len();
        }
        found
    }

    /// The last row before `to` with a warning or an error, for
    /// **Previous**. Edited rows are marked as they read now; deleted rows
    /// are passed over.
    #[must_use]
    pub fn previous_row_with_diagnostic(&self, to: usize) -> Option<usize> {
        let reading = self.current();
        let diagnostics = reading.diagnostics.get()?;
        let overlay = reading.edits.overlay();
        if overlay.is_empty() {
            return diagnostics.previous_row_with_diagnostic(to);
        }
        let map = overlay.map();
        let Some(len) = map.len() else {
            return Self::previous_marked(&reading, diagnostics, &overlay, 0..to);
        };
        // As for Next: one search backward through the file's own rows.
        let to = to.min(len);
        let mode = folded(&overlay, diagnostics.marked_rows_and_mode().1);
        let mut physical = map.physical_at_or_after(to);
        let found = loop {
            let Some(row) = Self::previous_marked(&reading, diagnostics, &overlay, 0..physical)
            else {
                break None;
            };
            let row = u32::try_from(row).unwrap_or(u32::MAX);
            match map.logical_of(row) {
                Ok(logical) => break Some(logical),
                Err(_) => match map.live_end_before(row) {
                    0 => break None,
                    end => physical = to_usize(end),
                },
            }
        };
        let begin = found.map_or(0, |logical| logical + 1);
        let mut logical = begin;
        let mut segments = Vec::new();
        for segment in map.segments(begin..to) {
            let len = segment.len();
            segments.push((logical, segment));
            logical += len;
        }
        for (logical, segment) in segments.into_iter().rev() {
            if let Segment::Inserted(range) = &segment {
                for (k, n) in range.clone().enumerate().rev() {
                    if inserted_flags(&reading, &overlay, n, mode).marked {
                        return Some(logical + k);
                    }
                }
            }
        }
        found
    }

    /// The first marked physical row among `rows`, edited rows as they
    /// read now.
    fn next_marked(
        reading: &Reading,
        diagnostics: &Diagnostics,
        overlay: &Overlay,
        rows: Range<usize>,
    ) -> Option<usize> {
        if !overlay.columns().is_empty() {
            return Self::folded_marked(reading, diagnostics, overlay, rows, true, None);
        }
        let (marked, mode) = diagnostics.marked_rows_and_mode();
        let mut at = rows.start;
        loop {
            // The next row the file's own marks give, and any edited row
            // up to it that is marked now.
            let next = diagnostics.next_row_with_diagnostic(at);
            let end = next.map_or(marked, |row| row + 1).min(marked).min(rows.end);
            if let Some((row, _)) = overlay
                .rows_in(at..end)
                .find(|&(row, edits)| Self::edited_flags(reading, overlay, row, edits, mode).marked)
            {
                return Some(row);
            }
            let row = next.filter(|&row| row < rows.end)?;
            if !overlay.contains(row) {
                return Some(row);
            }
            at = row + 1;
        }
    }

    /// The last marked physical row among `rows`, edited rows as they read
    /// now.
    fn previous_marked(
        reading: &Reading,
        diagnostics: &Diagnostics,
        overlay: &Overlay,
        rows: Range<usize>,
    ) -> Option<usize> {
        if !overlay.columns().is_empty() {
            return Self::folded_marked(reading, diagnostics, overlay, rows, false, None);
        }
        let (marked, mode) = diagnostics.marked_rows_and_mode();
        let mut at = rows.end;
        loop {
            let previous = diagnostics.previous_row_with_diagnostic(at);
            let start = previous.unwrap_or(0).max(rows.start);
            if let Some((row, _)) = overlay
                .rows_in(start..at.min(marked))
                .rev()
                .find(|&(row, edits)| Self::edited_flags(reading, overlay, row, edits, mode).marked)
            {
                return Some(row);
            }
            let row = previous.filter(|&row| row >= rows.start)?;
            if !overlay.contains(row) {
                return Some(row);
            }
            at = row;
        }
    }

    /// Each of rows `rows`' marks (task 1.7): whether its gutter has a
    /// marker, and whether it is ragged, so its missing cells are hatched
    /// (ADR-0002 questions 5 and 7). Rows not indexed yet are unmarked.
    /// Edited rows are marked as they read now: a short row whose hatched
    /// cells were filled in up to the common field count is no longer
    /// ragged, for example. Inserted rows are marked from their cells.
    #[must_use]
    pub fn row_flags(&self, rows: Range<usize>) -> Vec<RowFlags> {
        let reading = self.current();
        let Some(diagnostics) = reading.diagnostics.get() else {
            return vec![RowFlags::default(); rows.len()];
        };
        let overlay = reading.edits.overlay();
        let map = overlay.map();
        if map.is_identity() {
            return Self::physical_flags(&reading, diagnostics, &overlay, rows);
        }
        let mode = folded(&overlay, diagnostics.marked_rows_and_mode().1);
        let mut flags = Vec::with_capacity(rows.len());
        for segment in map.segments(rows.clone()) {
            match segment {
                Segment::Original(range) => flags.extend(Self::physical_flags(
                    &reading,
                    diagnostics,
                    &overlay,
                    to_usize(range.start)..to_usize(range.end),
                )),
                Segment::Inserted(range) => {
                    flags.extend(range.map(|n| inserted_flags(&reading, &overlay, n, mode)));
                }
            }
        }
        flags.resize(rows.len(), RowFlags::default());
        flags
    }

    /// [`row_flags`](Self::row_flags) of physical rows `rows`.
    fn physical_flags(
        reading: &Reading,
        diagnostics: &Diagnostics,
        overlay: &Overlay,
        rows: Range<usize>,
    ) -> Vec<RowFlags> {
        let mut flags = diagnostics.row_flags(rows.clone());
        if !overlay.columns().is_empty() {
            let marks = FoldedMarks::new(overlay.columns(), diagnostics);
            for (row, flag) in rows.clone().zip(flags.iter_mut()) {
                *flag = match diagnostics.code_of(row) {
                    Some(code) if !overlay.contains(row) => {
                        marks.flags(reading, overlay, row, code)
                    }
                    _ => RowFlags::default(),
                };
            }
        }
        if !overlay.is_empty() {
            let (marked, mode) = diagnostics.marked_rows_and_mode();
            for (row, edits) in overlay.rows_in(rows.start..rows.end.min(marked)) {
                flags[row - rows.start] = Self::edited_flags(reading, overlay, row, edits, mode);
            }
        }
        flags
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
        let overlay = reading.edits.overlay();
        let step = |physical: usize| {
            self.step_physical(
                &reading,
                diagnostics,
                &report,
                &overlay,
                kind,
                physical,
                direction,
                search,
            )
        };
        let map = overlay.map();
        let Some(len) = map.len() else {
            return step(start);
        };
        // Rows inserted or deleted (task 2.4a): the file's rows from where
        // `start` is among them, passing over deleted ones, then any
        // inserted row between `start` and what they found.
        let mode = folded(&overlay, diagnostics.marked_rows_and_mode().1);
        let inserted = |n: u32| inserted_place(&reading, &overlay, n, kind, mode);
        // An inserted row has only these kinds: for the others, nothing
        // needs walking.
        let in_inserted = matches!(kind, DiagnosticKind::RaggedRows | DiagnosticKind::NulBytes);
        let rows = map.physical_rows().unwrap_or(0);
        if direction == Direction::Forward {
            let mut physical = map.physical_at_or_after(start);
            let found = loop {
                let Some(place) = step(physical)? else {
                    break None;
                };
                let row = u32::try_from(place.row).unwrap_or(u32::MAX);
                match map.logical_of(row) {
                    Ok(logical) => break Some((logical, place.column)),
                    Err(_) if row < rows => physical = to_usize(map.next_live(row + 1)),
                    Err(_) => break None,
                }
            };
            let end = if in_inserted {
                found.map_or(len, |(logical, _)| logical)
            } else {
                start
            };
            let mut logical = start;
            for segment in map.segments(start..end) {
                if let Segment::Inserted(range) = &segment {
                    for (k, n) in range.clone().enumerate() {
                        if let Some(column) = inserted(n) {
                            let row = logical + k;
                            return Ok(Some(Place { row, column }));
                        }
                    }
                }
                logical += segment.len();
            }
            Ok(found.map(|(row, column)| Place { row, column }))
        } else {
            let to = start.min(len);
            let mut physical = map.physical_at_or_after(to);
            let found = loop {
                let Some(place) = step(physical)? else {
                    break None;
                };
                let row = u32::try_from(place.row).unwrap_or(u32::MAX);
                match map.logical_of(row) {
                    Ok(logical) => break Some((logical, place.column)),
                    Err(_) => match map.live_end_before(row) {
                        0 => break None,
                        end => physical = to_usize(end),
                    },
                }
            };
            let begin = if in_inserted {
                found.map_or(0, |(logical, _)| logical + 1)
            } else {
                to
            };
            let mut logical = begin;
            let mut segments = Vec::new();
            for segment in map.segments(begin..to) {
                let len = segment.len();
                segments.push((logical, segment));
                logical += len;
            }
            for (logical, segment) in segments.into_iter().rev() {
                if let Segment::Inserted(range) = &segment {
                    for (k, n) in range.clone().enumerate().rev() {
                        if let Some(column) = inserted(n) {
                            let row = logical + k;
                            return Ok(Some(Place { row, column }));
                        }
                    }
                }
            }
            Ok(found.map(|(row, column)| Place { row, column }))
        }
    }

    /// [`step_to_kind`](Self::step_to_kind) over the file's rows, by
    /// physical row: from physical row `start`, the place's row physical.
    #[allow(clippy::too_many_arguments)]
    fn step_physical(
        &self,
        reading: &Reading,
        diagnostics: &Diagnostics,
        report: &Report,
        overlay: &Overlay,
        kind: DiagnosticKind,
        start: usize,
        direction: Direction,
        search: u64,
    ) -> Result<Option<Place>, ReadError> {
        if overlay.is_empty() {
            return self.search_kind(reading, diagnostics, report, kind, start, direction, search);
        }
        let forward = direction == Direction::Forward;
        if !overlay.columns().is_empty() {
            // Columns inserted or deleted (task 2.4b): each row's marks as
            // it reads now, from its field count, reading the flagged ones.
            if !FoldedMarks::navigates(kind) {
                return Ok(None);
            }
            let rows = if forward { start..usize::MAX } else { 0..start };
            let found =
                Self::folded_marked(reading, diagnostics, overlay, rows, forward, Some(kind));
            return match found {
                Some(row) => Self::place(reading, kind, row),
                None => Ok(None),
            };
        }
        // With edits: the next occurrence in the file's own rows, unless an
        // edited row before it has the kind now. An edited row the file's
        // rows give is passed over, since it was checked as it reads now.
        let (marked, _) = diagnostics.marked_rows_and_mode();
        let has =
            |row: usize, edits: &RowEdits| Self::edited_has(reading, overlay, row, edits, kind);
        let mut at = start;
        loop {
            let found =
                self.search_kind(reading, diagnostics, report, kind, at, direction, search)?;
            let edited = if forward {
                let end = found.map_or(marked, |place| place.row + 1).min(marked);
                overlay
                    .rows_in(at..end)
                    .find(|&(row, edits)| has(row, edits))
            } else {
                let start = found.map_or(0, |place| place.row);
                overlay
                    .rows_in(start..at.min(marked))
                    .rev()
                    .find(|&(row, edits)| has(row, edits))
            };
            if let Some((row, _)) = edited {
                return Self::place(reading, kind, row);
            }
            let Some(place) = found else {
                return Ok(None);
            };
            if !overlay.contains(place.row) {
                return Ok(Some(place));
            }
            at = if forward { place.row + 1 } else { place.row };
        }
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
            return Self::place(reading, kind, row);
        }

        // 2. The bytes.
        let encoding = reading.detection.encoding;
        if direction == Direction::Forward
            && let Some(bytes) = reading.source.as_slice()
            && let Some(extent) = reading.index.row_extent(start)
        {
            match next_hit(kind, encoding, bytes, extent.start, bytes.len(), &stopped) {
                Hit::At(offset) => match reading.index.row_at_offset(offset) {
                    Some(row) if diagnostics.row_is(row, Mark::Flagged) => {
                        return Self::place(reading, kind, row);
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
                if alone || Self::row_has(reading, kind, row)? {
                    return Self::place(reading, kind, row);
                }
            }
            // `rows_where` looks before `at` going backward.
            at = if forward { last + 1 } else { last };
        }
    }

    /// The bytes of row `row`, line ending included, from whichever index
    /// holds it ([`rows_index`](Self::rows_index)), without the row cache.
    fn row_bytes<'r>(
        reading: &'r Reading,
        row: usize,
    ) -> Result<Option<RowBytes<'r, 'r>>, ReadError> {
        let stale = reading.head_is_stale();
        let (index, available) = reading.rows_from(&reading.index, stale);
        if row >= available {
            return Ok(None);
        }
        let Some(extent) = index.row_extent(row) else {
            return Ok(None);
        };
        let bytes = reading.bytes_of(extent.clone(), stale)?;
        Ok(Some(RowBytes {
            bytes,
            base: extent.start,
            index,
        }))
    }

    /// Whether row `row` has an occurrence of the field-level `kind`.
    fn row_has(reading: &Reading, kind: DiagnosticKind, row: usize) -> Result<bool, ReadError> {
        let Some(RowBytes { bytes, base, index }) = Self::row_bytes(reading, row)? else {
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
    /// first extra one (a long row); otherwise the first cell with the
    /// kind. An edited row is looked at as it reads now.
    fn place(
        reading: &Reading,
        kind: DiagnosticKind,
        row: usize,
    ) -> Result<Option<Place>, ReadError> {
        let overlay = reading.edits.overlay();
        let column = match Self::row_bytes(reading, row)? {
            Some(RowBytes { bytes, base, index }) => reading
                .parser
                .parse_row_in(index, row, &bytes, base)
                .and_then(|parsed| {
                    let view = RowView::new(
                        &reading.parser,
                        &bytes,
                        base,
                        &parsed,
                        overlay.physical(row),
                    );
                    if kind == DiagnosticKind::RaggedRows {
                        let cells = view.len();
                        let mode = folded(&overlay, reading.index.field_count_mode());
                        Some(mode.map_or(cells, |mode| cells.min(mode)))
                    } else {
                        view.first_with(kind)
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
        self.current().source.can_save()
            && self.original.status().state != OriginalState::Unavailable
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
        // The current reading's source, whichever reading that is then: a
        // save replaces it (task 2.2).
        let reading = Arc::downgrade(&self.reading);
        // The thread gets its quality of service from the platform, as the
        // scheduler's own threads do (`ThreadClass::Watcher`).
        let platform = self.scheduler.platform();
        let started = move || platform.thread_started(ThreadClass::Watcher);
        self.original.watch_on(started, move |status| {
            if let Some(reading) = reading.upgrade() {
                let source = Arc::clone(
                    &reading
                        .read()
                        .unwrap_or_else(PoisonError::into_inner)
                        .source,
                );
                source.note_original_path(&status.path);
                if changes_what_is_read(&source, status) {
                    source.note_original_written();
                }
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
        self.check_original_restarting().0
    }

    /// [`check_original`](Self::check_original), saying whether it read the
    /// file again, and from which generation to which: the app's cached
    /// first screen is relabelled only for its own restart (a save may
    /// have replaced the reading meanwhile). While a save runs, a
    /// disconnected document isn't reconnected; the save checks again when
    /// it ends ([`SaveJob::restarted`]).
    pub fn check_original_restarting(&self) -> (OriginalStatus, Option<Restarted>) {
        // It looks at the file: never on the main thread for a share.
        let source = self.source();
        source.note_share_use();
        let status = self.original.check();
        source.note_original_path(&status.path);
        if changes_what_is_read(&source, &status) {
            source.note_original_written();
        }
        let back = matches!(
            status.state,
            OriginalState::Unchanged | OriginalState::Changed
        );
        let restarted = if back && !status.diverged && source.storage() == Storage::Disconnected {
            self.reconnect_and_restart(&source, &status.path)
        } else {
            None
        };
        (status, restarted)
    }

    /// Whether the file changed on its drive while Leal was reading it
    /// ([`Source::changed_on_disk`]): the rows held may mix two versions.
    #[must_use]
    pub fn changed_on_disk(&self) -> bool {
        self.current().source.changed_on_disk()
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
        // The watcher's descriptor on a share is closed on a thread of its
        // own, as the source's file is (task 2.0 review): closing a file on
        // a share that has stopped answering can block, and the app may let
        // go of a document anywhere.
        let source = self.source();
        if source.is_on_network_share() {
            self.original.close_on_own_thread(source.share_close_hook());
        }
    }
}

impl fmt::Debug for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Document")
            .field("source", &self.current().source)
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}

impl Reading {
    /// Whether the first 64 KB kept in memory may be a different version
    /// from the rest (task 1.9). The file changed while it was read without
    /// a snapshot: first paint read the 64 KB before the copy began, and a
    /// change that kept the size and modification time could have come in
    /// between, so from then on rows come only from the index pass's copy,
    /// whose every chunk was checked before it was kept.
    fn head_is_stale(&self) -> bool {
        self.source.changed_on_disk()
    }

    /// [`Document::reinterpret`]'s and `restart`'s refusal once the first
    /// 64 KB kept in memory can't be trusted.
    fn refuse_once_stale(&self) -> Result<(), DocumentError> {
        if self.head_is_stale() {
            Err(DocumentError::Read(ReadError::changed_on_disk()))
        } else {
            Ok(())
        }
    }

    /// The bytes of `extent` ([`bytes_in`]), with the first 64 KB trusted
    /// unless `head_stale`.
    fn bytes_of(&self, extent: Range<usize>, head_stale: bool) -> Result<Cow<'_, [u8]>, ReadError> {
        Ok(bytes_in(&self.source, &self.head, extent, head_stale)?.0)
    }

    /// The index rows are read from, and how many rows it has: `index`
    /// (the reading's own, or a copy job's) or the first 64 KB's, whichever
    /// covers more of the file's first rows (both cover a prefix), except
    /// that once the first 64 KB are stale (`head_stale`, the file changed
    /// while it was read), only `index`.
    ///
    /// Every reader of rows goes through this and [`bytes_in`]: the grid
    /// and the inspector, Copy and Find (p1-review fid-1 and fid-2). So
    /// after a removable drive vanished early, Copy and Find still have
    /// the rows of the first 64 KB that the grid shows, and after a change
    /// none of them reads the stale 64 KB.
    fn rows_from<'r>(&'r self, index: &'r RowIndex, head_stale: bool) -> (&'r RowIndex, usize) {
        let indexed = index.row_count();
        if indexed >= self.head_rows || head_stale {
            (index, indexed)
        } else {
            (&self.head_index, self.head_rows)
        }
    }

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

/// The bytes of `extent`, and whether they came from the first 64 KB kept
/// in memory (`head`): from there if it holds them and `head_stale` is
/// false, otherwise one read of `source`. Once the file changed while it
/// was read without a snapshot ([`Source::changed_on_disk`]), the first
/// 64 KB may be a different version from the rest, so from then on bytes
/// come only from the checked copy (task 1.9; for every reader since
/// p1-review fid-2).
///
/// The caller looks at `changed_on_disk` once and passes the same answer
/// here and to [`Reading::rows_from`], so the rows of one read and their
/// bytes agree. A job that keeps what it reads (Copy, Find) uses the
/// second value to know it holds rows from the first 64 KB, which it must
/// drop if they turn out stale before it finishes.
fn bytes_in<'a>(
    source: &'a Source,
    head: &'a [u8],
    extent: Range<usize>,
    head_stale: bool,
) -> Result<(Cow<'a, [u8]>, bool), ReadError> {
    match head.get(extent.clone()) {
        Some(bytes) if !head_stale => Ok((Cow::Borrowed(bytes), true)),
        _ => Ok((source.read_range(extent)?, false)),
    }
}

/// Whether `status` means the file read without a snapshot changed under
/// the copy ([`Source::note_original_written`]): a write the watcher saw,
/// or, for a file on a network share, any change a look found. A share's
/// watcher doesn't see another computer's writes, so on a share a change
/// found by `check` (on activation, a mount, the periodic check) is the
/// only sign, and the rest of the copy would be the new version
/// (task 2.0 review). Once the copy is complete it does nothing.
fn changes_what_is_read(source: &Source, status: &OriginalStatus) -> bool {
    status.written || (source.is_on_network_share() && status.state == OriginalState::Changed)
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

/// The first screen's rows, from the first 64 KB alone, with `edits` (a
/// reading that keeps the edits: a new header choice).
fn first_screen(
    head: &[u8],
    len: u64,
    generation: u64,
    paint: &FirstPaint,
    options: OpenOptions,
    edits: &Overlay,
) -> FirstScreen {
    // Logical rows, through the piece list (rows inserted or deleted).
    let map = edits.map();
    let row_count = map.rows_within(paint.head_rows);
    let mut rows = Vec::new();
    for segment in map.segments(0..row_count.min(options.first_screen_rows)) {
        match segment {
            Segment::Original(range) => {
                rows.extend(range.filter_map(|r| {
                    let r = to_usize(r);
                    let row = paint.parser.parse_row(&paint.head_index, r, head)?;
                    let view = RowView::new(&paint.parser, head, 0, &row, edits.physical(r));
                    Some(row_cells(&view, options.max_chars))
                }));
            }
            Segment::Inserted(range) => {
                rows.extend(range.filter_map(|n| {
                    let (row, cells) = inserted_row(edits, n)?;
                    let view = RowView::inserted(&paint.parser, row, cells);
                    Some(row_cells(&view, options.max_chars))
                }));
            }
        }
    }
    let estimate = estimate_from_head(&paint.head_index, paint.head_rows, head, len);
    FirstScreen {
        generation,
        detection: paint.detection.clone(),
        rows,
        row_count,
        estimated_row_count: map.shift(estimate),
        column_count: if map.len() == Some(0) {
            0
        } else {
            folded(edits, paint.head_index.field_count_mode()).unwrap_or(0)
        },
    }
}

/// Starts the index (P1) and the checks (P2) for a reading.
fn start_jobs(
    context: &Context<'_>,
    generation: u64,
    paint: FirstPaint,
    choices: Choices,
    edits: Arc<EditStore>,
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
        (&index, &edits),
        indexer,
        detection.encoding,
        &diagnostics,
    );
    let review_job = start_checks(context, &index_job, &detection);
    Reading {
        source: Arc::clone(context.source),
        head: Arc::clone(context.head),
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
        edits,
        counts: None,
        saved: false,
        remembered: AtomicBool::new(false),
        widest: Mutex::new(None),
    }
}

/// The edits a new reading of the file starts with, after `old`: `old`'s,
/// if the new one splits the file into cells the same way (the same
/// delimiter, encoding and BOM: a new header choice, or the same choices
/// again after a drive came back), otherwise none, which is allowed only if
/// `old` has none (ADR-0008 decision 4).
fn edits_after(old: &Reading, detection: &Detection) -> Result<Arc<EditStore>, DocumentError> {
    let same_split = old.detection.delimiter == detection.delimiter
        && old.detection.encoding == detection.encoding
        && old.detection.bom == detection.bom;
    if same_split {
        Ok(Arc::clone(&old.edits))
    } else if old.edits.is_empty() {
        // A new lineage, whose versions carry on from the old edits'.
        let edits = EditStore::default();
        edits.carry_on_from(old.edits.version());
        Ok(Arc::new(edits))
    } else {
        Err(DocumentError::UnsavedEdits)
    }
}

/// P1: the row index, on its own thread, collecting the diagnostics of
/// text in `encoding` as it goes. It makes them first, on that thread, and
/// puts them in `diagnostics` for readers.
fn start_index(
    context: &Context<'_>,
    generation: u64,
    (index, edits): (&Arc<RowIndex>, &Arc<EditStore>),
    indexer: Indexer,
    encoding: Encoding,
    diagnostics: &DiagnosticsSlot,
) -> JobHandle<IndexSummary> {
    let source = Arc::clone(context.source);
    let progress = context.progress.cloned();
    let readers = Arc::clone(index);
    // A save's new reading serves its rows from an index built from the
    // save's plan (task 2.4c), complete at once; the pass then fills an
    // index of its own, for the diagnostics and the field count mode.
    let filled = Arc::clone(indexer.index());
    let edits = Arc::clone(edits);
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
                    progress(index_progress(generation, &readers, p, &edits));
                }
                checkpoint
            };
            index_file(&source, indexer, job, report)?;
            if !Arc::ptr_eq(&filled, &readers) {
                readers.adopt_field_count_mode(&filled);
            }
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
/// (or its share stopped answering) before the copy was complete, it was
/// deleted on its share, or it changed while it was read.
fn unavailable(source: &Source) -> JobError {
    if source.changed_on_disk() {
        JobError::Read(ReadErrorKind::ChangedOnDisk)
    } else if source.storage() == Storage::Disconnected {
        JobError::Read(ReadErrorKind::Disconnected)
    } else if source.storage() == Storage::Deleted {
        JobError::Read(ReadErrorKind::Deleted)
    } else {
        JobError::Failed("the whole file isn't available to review".to_owned())
    }
}

impl Document {
    /// How many cells edited row `row` has now, as its `RowView` counts
    /// them ([`RowView::len`]). While the first 64 KB are trusted that is
    /// the count kept with its edits (laid out under the column inserts and
    /// deletes), made from the same bytes as any read of the row now, so no
    /// row is read. Once they are stale, the row is read from the trusted
    /// copy (it may not be there: then the kept count).
    fn edited_len(reading: &Reading, overlay: &Overlay, row: usize, edits: &RowEdits) -> usize {
        let kept = || edits.len_in(overlay.columns(), None);
        if !reading.head_is_stale() {
            return kept();
        }
        Self::read_rows_of(reading, row..row + 1, |view| view.len())
            .ok()
            .and_then(|lens| lens.first().copied())
            .unwrap_or_else(kept)
    }

    /// Edited row `row`'s marks as it reads now, against the most common
    /// field count `mode` (as the file has it: folded here). An edited row
    /// is blank only if its one cell is its own, unedited blank field.
    fn edited_flags(
        reading: &Reading,
        overlay: &Overlay,
        row: usize,
        edits: &RowEdits,
        mode: Option<usize>,
    ) -> RowFlags {
        let columns = overlay.columns();
        let len = Self::edited_len(reading, overlay, row, edits);
        let blank = edits.is_blank_in(columns, None);
        let ragged = !blank && folded(overlay, mode).is_some_and(|mode| len != mode);
        RowFlags {
            marked: ragged || !edits.kinds_in(columns, original_id(row)).is_empty(),
            ragged,
        }
    }

    /// Whether edited row `row` has `kind` as it reads now.
    fn edited_has(
        reading: &Reading,
        overlay: &Overlay,
        row: usize,
        edits: &RowEdits,
        kind: DiagnosticKind,
    ) -> bool {
        match kind {
            DiagnosticKind::RaggedRows => {
                let mode = reading
                    .diagnostics
                    .get()
                    .and_then(|d| d.marked_rows_and_mode().1);
                Self::edited_flags(reading, overlay, row, edits, mode).ragged
            }
            other => edits
                .kinds_in(overlay.columns(), original_id(row))
                .contains(Kinds::of(other)),
        }
    }

    /// With columns inserted or deleted (task 2.4b): the first (`forward`)
    /// or last physical row among `rows` that is marked now, or has `kind`.
    /// Candidate rows come from the marks a batch at a time (by their field
    /// counts' default layouts, and their flags), with every edited row
    /// between; an unedited flagged row is read, since a column delete may
    /// have taken its flagged field.
    fn folded_marked(
        reading: &Reading,
        diagnostics: &Diagnostics,
        overlay: &Overlay,
        rows: Range<usize>,
        forward: bool,
        kind: Option<DiagnosticKind>,
    ) -> Option<usize> {
        let marks = FoldedMarks::new(overlay.columns(), diagnostics);
        let (marked, mode) = diagnostics.marked_rows_and_mode();
        let end = rows.end.min(marked);
        if rows.start >= end {
            return None;
        }
        let edited_has = |row: usize, edits: &RowEdits| match kind {
            None => Self::edited_flags(reading, overlay, row, edits, mode).marked,
            Some(kind) => Self::edited_has(reading, overlay, row, edits, kind),
        };
        let pick = |code: RowCode| marks.may_be(code, kind);
        let mut at = if forward { rows.start } else { end };
        loop {
            let batch = diagnostics.rows_picked(at, forward, SEARCH_BATCH, &pick);
            let full = batch.len() == SEARCH_BATCH;
            // The rows this batch covers: up to its last candidate if it
            // is full, otherwise to the end of `rows`.
            let covered = match (forward, full, batch.last()) {
                (true, true, Some(&(row, _))) => at..(row + 1).min(end),
                (true, _, _) => at..end,
                (false, true, Some(&(row, _))) => row.max(rows.start)..at,
                (false, _, _) => rows.start..at,
            };
            let mut candidates: Vec<usize> = batch
                .iter()
                .map(|&(row, _)| row)
                .filter(|row| covered.contains(row) && !overlay.contains(*row))
                .collect();
            candidates.extend(overlay.rows_in(covered.clone()).map(|(row, _)| row));
            candidates.sort_unstable();
            if !forward {
                candidates.reverse();
            }
            for row in candidates {
                let hit = match overlay.row(row) {
                    Some(edits) => edited_has(row, edits),
                    None => diagnostics
                        .code_of(row)
                        .is_some_and(|code| marks.row_is(reading, overlay, row, code, kind)),
                };
                if hit {
                    return Some(row);
                }
            }
            if !full || covered.is_empty() {
                return None;
            }
            at = if forward { covered.end } else { covered.start };
            if (forward && at >= end) || (!forward && at <= rows.start) {
                return None;
            }
        }
    }
}

/// How unedited rows are marked once columns are inserted or deleted (task
/// 2.4b): a row's length is its field count's default layout's, and its
/// inserted cells may hold a NUL, both decided per field count once; its
/// own flagged fields may have gone with a deleted column, so a flagged row
/// is read.
struct FoldedMarks<'c> {
    columns: &'c Columns,
    /// The most common field count, folded.
    mode: Option<usize>,
    /// For each field count below [`TABLE`]: (ragged, an inserted cell
    /// holds a NUL).
    table: Vec<(bool, bool)>,
    /// Some row's cell put back by value may hold a NUL: rows are read.
    reads: bool,
}

impl<'c> FoldedMarks<'c> {
    fn new(columns: &'c Columns, diagnostics: &Diagnostics) -> FoldedMarks<'c> {
        let mode = diagnostics
            .marked_rows_and_mode()
            .1
            .map(|mode| columns.fold_fields(mode));
        let reads = columns
            .ops()
            .iter()
            .any(|op| op.value().is_none() && op.values().any(|value| value.contains('\0')));
        let mut marks = FoldedMarks {
            columns,
            mode,
            table: Vec::new(),
            reads,
        };
        marks.table = (0..TABLE).map(|n| marks.by_count(n)).collect();
        marks
    }

    /// Whether Previous and Next look for `kind`: the field-level kinds and
    /// ragged rows.
    fn navigates(kind: DiagnosticKind) -> bool {
        matches!(
            kind,
            DiagnosticKind::RaggedRows
                | DiagnosticKind::UnterminatedQuote
                | DiagnosticKind::TextAfterClosingQuote
                | DiagnosticKind::InvalidEncoding
                | DiagnosticKind::NulBytes
        )
    }

    /// (ragged, an inserted cell holds a NUL) for a non-blank row of `n`
    /// fields. A cell put back by value holds each row's own value: a row
    /// that may have a NUL there is read ([`reads`](Self::reads)).
    fn by_count(&self, n: usize) -> (bool, bool) {
        if let Some(&entry) = self.table.get(n) {
            return entry;
        }
        let own = EditOwn::original(n, false);
        let fold = self.columns.fold(own);
        let ragged = self.mode.is_some_and(|mode| fold.len() != mode);
        let nul = fold.as_slice().iter().any(|&id| match id {
            CellId::Inserted(op) => self
                .columns
                .op(op)
                .and_then(|op| op.value())
                .is_some_and(|value| value.contains('\0')),
            _ => false,
        });
        (ragged, nul)
    }

    /// Whether a row with `code` may be marked (`kind` none) or have
    /// `kind`: then it is checked ([`row_is`](Self::row_is)).
    fn may_be(&self, code: RowCode, kind: Option<DiagnosticKind>) -> bool {
        let (ragged, nul) = code.fields.map_or((false, false), |n| self.by_count(n));
        let nul_maybe = nul || self.reads;
        match kind {
            None => ragged || nul_maybe || code.flagged,
            Some(DiagnosticKind::RaggedRows) => ragged,
            Some(DiagnosticKind::NulBytes) => nul_maybe || code.flagged,
            Some(_) => code.flagged,
        }
    }

    /// Whether unedited physical row `row`, with `code`, is marked
    /// (`kind` none) or has `kind` now. A flagged row is read.
    fn row_is(
        &self,
        reading: &Reading,
        overlay: &Overlay,
        row: usize,
        code: RowCode,
        kind: Option<DiagnosticKind>,
    ) -> bool {
        let (ragged, nul) = code.fields.map_or((false, false), |n| self.by_count(n));
        let reads = code.flagged || self.reads;
        let read =
            |kind: Option<DiagnosticKind>| reads && Self::read_has(reading, overlay, row, kind);
        match kind {
            None => ragged || nul || read(None),
            Some(DiagnosticKind::RaggedRows) => ragged,
            Some(DiagnosticKind::NulBytes) => nul || read(Some(DiagnosticKind::NulBytes)),
            Some(kind) => read(Some(kind)),
        }
    }

    /// Unedited physical row `row`'s marks now.
    fn flags(&self, reading: &Reading, overlay: &Overlay, row: usize, code: RowCode) -> RowFlags {
        let (ragged, _) = code.fields.map_or((false, false), |n| self.by_count(n));
        RowFlags {
            marked: self.row_is(reading, overlay, row, code, None),
            ragged,
        }
    }

    /// Whether row `row`, read, has a field-level kind (`kind`, or any) in
    /// a cell it still has.
    fn read_has(
        reading: &Reading,
        overlay: &Overlay,
        row: usize,
        kind: Option<DiagnosticKind>,
    ) -> bool {
        let found = Document::read_physical(reading, overlay, row..row + 1, &mut |view| {
            let kinds = Kinds::FIELD_KINDS
                .iter()
                .filter(|&&(k, _)| kind.is_none_or(|kind| kind == k));
            kinds
                .into_iter()
                .any(|&(k, _)| view.first_with(k).is_some())
        });
        found.is_ok_and(|found| found.first().copied().unwrap_or(false))
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

/// Every cell of a row as it reads now. A row of the file with no edits
/// takes the same path as before edits existed ([`cells`]).
fn row_cells(view: &RowView<'_>, max_chars: usize) -> Vec<Cell> {
    if let Some(parsed) = view.plain() {
        return cells(
            view.parser(),
            view.bytes(),
            view.base(),
            parsed.fields(),
            max_chars,
        );
    }
    edited_cells(view, 0..view.len(), max_chars).0
}

/// Cells `columns` of an edited row, as it reads now, and which of those
/// columns hold an edit.
fn edited_cells(
    view: &RowView<'_>,
    columns: Range<usize>,
    max_chars: usize,
) -> (Vec<Cell>, Vec<usize>) {
    let mut edited = Vec::new();
    let cells = columns
        .filter_map(|column| {
            let cell = view.cell(column)?;
            if matches!(cell, ViewCell::Edited(_)) {
                edited.push(column);
            }
            let (text, truncated) = view.prefix_of(cell, max_chars);
            Some(Cell {
                text: text.into_owned(),
                truncated,
            })
        })
        .collect();
    (cells, edited)
}

/// A progress report for the app, in logical rows (`edits`' rows inserted
/// and deleted).
fn index_progress(
    generation: u64,
    index: &RowIndex,
    progress: Progress,
    edits: &EditStore,
) -> IndexProgress {
    let complete = index.status() == Status::Complete;
    // A save's new reading has every row at once (its index is built from
    // the save's plan) while its index pass runs for the diagnostics.
    let rows = if complete {
        index.row_count()
    } else {
        progress.rows
    };
    IndexProgress {
        generation,
        rows: edits.rows_within(rows),
        estimated_rows: edits.shift(index.estimated_row_count().unwrap_or(0).max(progress.rows)),
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

/// Inserted row `n` and what lays out its cells, if it is in the document.
fn inserted_row(overlay: &Overlay, n: u32) -> Option<(&InsertedRow, OverlayRow<'_>)> {
    let row = overlay.inserted(n)?;
    Some((row.as_ref(), overlay.cells_of(RowId::inserted(n))))
}

/// The most common field count `mode` as a row of that many fields reads
/// after the column inserts and deletes: the column count, and what a
/// row's length is ragged against.
fn folded(overlay: &Overlay, mode: Option<usize>) -> Option<usize> {
    mode.map(|mode| overlay.columns().fold_fields(mode))
}

/// Inserted row `n`'s marks (task 2.4a), from its cells as they read now,
/// against the most common field count `mode`: ragged if its length
/// differs, marked if ragged or it holds a NUL.
fn inserted_flags(reading: &Reading, overlay: &Overlay, n: u32, mode: Option<usize>) -> RowFlags {
    let Some((row, cells)) = inserted_row(overlay, n) else {
        return RowFlags::default();
    };
    let view = RowView::inserted(&reading.parser, row, cells);
    let ragged = mode.is_some_and(|mode| view.len() != mode);
    RowFlags {
        marked: ragged || view.first_with(DiagnosticKind::NulBytes).is_some(),
        ragged,
    }
}

/// Where `kind` is in inserted row `n`, if it is: the column to select, as
/// [`Document::place`] says it.
fn inserted_place(
    reading: &Reading,
    overlay: &Overlay,
    n: u32,
    kind: DiagnosticKind,
    mode: Option<usize>,
) -> Option<usize> {
    let (row, cells) = inserted_row(overlay, n)?;
    let view = RowView::inserted(&reading.parser, row, cells);
    match kind {
        DiagnosticKind::RaggedRows => {
            let cells = view.len();
            mode.filter(|&mode| cells != mode)
                .map(|mode| cells.min(mode))
        }
        DiagnosticKind::NulBytes => view.first_with(kind),
        _ => None,
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Physical row `row`'s id.
fn original_id(row: usize) -> RowId {
    RowId::original(u32::try_from(row).unwrap_or(u32::MAX))
}
