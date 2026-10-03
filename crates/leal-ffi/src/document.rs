//! Documents, jobs and the scheduler, for Swift (task 1.3a). See
//! [`leal_core::document`] and [`leal_core::schedule`].
//!
//! - [`Scheduler`]: one per app. The app reports input through it, which
//!   pauses background work (DESIGN §3.10 rule 3).
//! - [`open_document`]: first paint, synchronously, then the index and
//!   review in the background. It returns a [`Document`] whose
//!   [`first_screen`](Document::first_screen) is ready.
//! - [`Document::rows`]: rows for the grid, synchronously (under 1 ms).
//! - [`Job`]: a background job's handle, with [`cancel`](Job::cancel) and
//!   an async [`wait`](Job::wait) (ADR-0005 decision 6). The work runs on
//!   Rust's own threads; the async function only reports completion.
//!   Swift's task cancellation doesn't reach Rust, so the Swift wrapper
//!   (task 1.6) calls `cancel()` from `withTaskCancellationHandler`.
//! - [`ProgressObserver`]: a Swift object told about indexing progress, on
//!   the index's thread.
//! - [`Document::diagnostics`] (task 1.5): the irregularities found so far,
//!   for the banner and details; [`Document::row_has_diagnostic`] and its
//!   next and previous for the gutter's markers.
//! - [`Document::watch_original`] (task 1.9): the user's file is watched
//!   for changes made elsewhere, and an [`OriginalObserver`] is told;
//!   [`Document::check_original`] looks again when a volume mounts.

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use leal_core::detect::{self, Choices, Detection};
use leal_core::dialect;
use leal_core::document::{self, DocumentError};
use leal_core::schedule::{self, JobControl, JobError, SchedulerConfig};
use leal_core::source::{self, ReadError, ReadErrorKind, TempFolders};

use crate::platform::MacPlatform;
use crate::{LealError, SourceStorage, TempLocations, VolumeInfo};

/// Runs the background work of every document (DESIGN §3.10): one per app.
#[derive(Debug, uniffi::Object)]
pub struct Scheduler {
    scheduler: schedule::Scheduler,
}

#[uniffi::export]
impl Scheduler {
    /// Starts the background pool, sized to the performance cores less
    /// one, with each thread's QoS set and `os_signpost` intervals for
    /// every job.
    ///
    /// The app makes its one scheduler as it starts, so this also raises
    /// the process's open-file limit from the Finder's 256
    /// (`platform::raise_open_file_limit`):
    /// each open document holds a few descriptors.
    ///
    /// # Errors
    ///
    /// [`LealError::Internal`] if the threads can't be started.
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, LealError> {
        // If it can't be raised, opening many documents fails later with
        // "too many open files", and watching says so.
        let _ = crate::platform::raise_open_file_limit();
        // The app's main thread draws: from now on, a network share used
        // there is a bug the core's debug builds catch (ADR-0009).
        source::forbid_share_use_on_main_thread();
        let config = SchedulerConfig {
            platform: Arc::new(MacPlatform),
            ..SchedulerConfig::default()
        };
        let scheduler = schedule::Scheduler::new(config).map_err(|error| LealError::Internal {
            message: error.to_string(),
        })?;
        Ok(Arc::new(Scheduler { scheduler }))
    }

    /// The user scrolled, typed or clicked: background work pauses until
    /// input has been idle for about 250 ms. Cheap; call it on every event.
    pub fn note_user_input(&self) {
        self.scheduler.note_user_input();
    }

    /// A gesture (a scroll with momentum, a drag) began (`true`) or ended
    /// (`false`). Background work stays paused while it lasts.
    pub fn set_interacting(&self, interacting: bool) {
        self.scheduler.set_interacting(interacting);
    }

    /// The number of threads in the background pool.
    #[must_use]
    pub fn background_threads(&self) -> u32 {
        u32::try_from(self.scheduler.background_threads()).unwrap_or(u32::MAX)
    }
}

/// Told about indexing progress, on the index's thread, after each chunk
/// (about every millisecond). Keep it short: hop to the main thread.
#[uniffi::export(with_foreign)]
pub trait ProgressObserver: Send + Sync {
    /// The index has got to `progress`.
    fn index_progressed(&self, progress: IndexProgress);
}

/// Told when the user's file changes, moves, is deleted or its volume goes
/// away (task 1.9), on a watching thread of the document's own. Keep it
/// short: hop to the main thread.
#[uniffi::export(with_foreign)]
pub trait OriginalObserver: Send + Sync {
    /// The file is now as `status` says.
    fn original_changed(&self, status: OriginalStatus);
}

/// What has happened to the user's file since it was opened. See
/// [`leal_core::source::OriginalState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum OriginalState {
    /// As it was when opened (perhaps moved).
    Unchanged,
    /// Changed, or replaced by another file: the app offers **Reload** and
    /// **Keep editing**.
    Changed,
    /// Deleted, or moved to the Trash.
    Deleted,
    /// Its volume isn't mounted. Save is refused until it is back.
    Unavailable,
}

/// The user's file as last seen. See
/// [`leal_core::source::OriginalStatus`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct OriginalStatus {
    /// What has happened to it.
    pub state: OriginalState,
    /// Where it is now: it follows renames.
    pub path: String,
    /// Whether it has changed or been deleted at any time since it was
    /// opened, even if the user chose **Keep editing** (for Save's check,
    /// phase 2).
    pub diverged: bool,
}

impl From<source::OriginalStatus> for OriginalStatus {
    fn from(status: source::OriginalStatus) -> Self {
        Self {
            state: match status.state {
                source::OriginalState::Unchanged => OriginalState::Unchanged,
                source::OriginalState::Changed => OriginalState::Changed,
                source::OriginalState::Deleted => OriginalState::Deleted,
                source::OriginalState::Unavailable => OriginalState::Unavailable,
            },
            path: status.path.to_string_lossy().into_owned(),
            diverged: status.diverged,
        }
    }
}

/// A field delimiter (DESIGN §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Delimiter {
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// Tab.
    Tab,
    /// `|`
    Pipe,
}

/// A text encoding Leal reads (ADR-0005 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum TextEncoding {
    /// UTF-8.
    Utf8,
    /// UTF-16, little-endian (read-only in v1).
    Utf16Le,
    /// UTF-16, big-endian (read-only in v1).
    Utf16Be,
    /// Windows-1252.
    Windows1252,
    /// Windows-1250.
    Windows1250,
    /// Windows-1251.
    Windows1251,
    /// Windows-1253.
    Windows1253,
    /// Windows-1254.
    Windows1254,
    /// Windows-1255.
    Windows1255,
    /// Windows-1256.
    Windows1256,
    /// Windows-1257.
    Windows1257,
    /// Windows-1258.
    Windows1258,
    /// ISO-8859-1.
    Iso8859_1,
    /// ISO-8859-2.
    Iso8859_2,
    /// ISO-8859-15.
    Iso8859_15,
    /// Mac Roman.
    MacRoman,
}

/// A line ending, for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LineEnding {
    /// `\n`
    Lf,
    /// `\r\n`
    Crlf,
    /// `\r` on its own.
    Cr,
}

/// Where the encoding came from, for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EncodingSource {
    /// The byte order mark.
    Bom,
    /// The `com.apple.TextEncoding` attribute.
    Attribute,
    /// Guessed from the bytes.
    Guess,
    /// Chosen by the user.
    User,
}

/// Where the delimiter or header choice came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DialectSource {
    /// Leal's interpretation attribute.
    Attribute,
    /// Guessed from the bytes.
    Guess,
    /// Chosen by the user.
    User,
}

/// An attribute Leal ignored, for the status bar note (ADR-0005 decision
/// 5). See [`leal_core::detect::Note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum InterpretationNote {
    /// `com.apple.TextEncoding` isn't `name;number`.
    TextEncodingUnreadable,
    /// `com.apple.TextEncoding` names an encoding Leal doesn't read.
    TextEncodingUnsupported {
        /// Its `CFStringEncoding` number.
        cf_string_encoding: u32,
    },
    /// `com.apple.TextEncoding` says UTF-16, but the file has no UTF-16 BOM.
    TextEncodingUtf16WithoutBom,
    /// `com.apple.TextEncoding` names a single-byte encoding in which some
    /// of the file's bytes don't decode.
    TextEncodingDoesNotDecode {
        /// The encoding the attribute names.
        encoding: TextEncoding,
    },
    /// Leal's interpretation attribute couldn't be read.
    InterpretationUnreadable,
    /// Leal's interpretation attribute has an encoding Leal doesn't read
    /// from a tag; the rest of it is kept.
    InterpretationEncodingIgnored,
    /// The file changed since Leal saved its interpretation attribute, and
    /// the remembered delimiter no longer fits it.
    InterpretationNotSensible {
        /// The remembered delimiter.
        delimiter: Delimiter,
    },
}

impl From<detect::Note> for InterpretationNote {
    fn from(note: detect::Note) -> Self {
        use detect::Note as Core;
        match note {
            Core::TextEncodingUnreadable => Self::TextEncodingUnreadable,
            Core::TextEncodingUnsupported { cf_string_encoding } => {
                Self::TextEncodingUnsupported { cf_string_encoding }
            }
            Core::TextEncodingUtf16WithoutBom => Self::TextEncodingUtf16WithoutBom,
            Core::TextEncodingDoesNotDecode { encoding } => Self::TextEncodingDoesNotDecode {
                encoding: encoding.into(),
            },
            Core::InterpretationUnreadable => Self::InterpretationUnreadable,
            Core::InterpretationEncodingIgnored => Self::InterpretationEncodingIgnored,
            Core::InterpretationNotSensible { delimiter } => Self::InterpretationNotSensible {
                delimiter: delimiter.into(),
            },
        }
    }
}

/// How the file is read. See [`leal_core::detect::Detection`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Interpretation {
    /// The encoding.
    pub encoding: TextEncoding,
    /// Where the encoding came from.
    pub encoding_source: EncodingSource,
    /// The delimiter.
    pub delimiter: Delimiter,
    /// Where the delimiter came from.
    pub delimiter_source: DialectSource,
    /// Whether the first row is the header row.
    pub header: bool,
    /// Where the header choice came from.
    pub header_source: DialectSource,
    /// The most common line ending in the first 64 KB, or `None` if no
    /// row there has one. [`ReviewResult::line_ending`] has the whole
    /// file's.
    pub line_ending: Option<LineEnding>,
    /// Attributes that were ignored, for the status bar.
    pub notes: Vec<InterpretationNote>,
    /// The encodings **Reopen with encoding…** may choose: those the
    /// file's BOM allows.
    pub encoding_choices: Vec<TextEncoding>,
}

/// How to open (or re-read) a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct OpenOptions {
    /// **Treat as**: the delimiter to use instead of detecting one.
    #[uniffi(default = None)]
    pub delimiter: Option<Delimiter>,
    /// The **Header row** toggle, instead of detecting it.
    #[uniffi(default = None)]
    pub header: Option<bool>,
    /// **Reopen with encoding…**: it must fit the file's BOM.
    #[uniffi(default = None)]
    pub encoding: Option<TextEncoding>,
    /// How many rows the first screen has.
    #[uniffi(default = 100)]
    pub first_screen_rows: u32,
    /// The most characters of each cell the first screen shows.
    #[uniffi(default = 256)]
    pub max_chars: u32,
}

/// One cell, as the grid shows it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Cell {
    /// The start of the cell's value.
    pub text: String,
    /// Whether the value has more than `text`.
    pub truncated: bool,
}

/// One row's cells in a window of columns. See
/// [`leal_core::document::RowCells`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RowCells {
    /// How many fields the whole row has.
    pub field_count: u32,
    /// The row's cells in the window.
    pub cells: Vec<Cell>,
}

/// First paint's result. See [`leal_core::document::FirstScreen`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FirstScreen {
    /// Which reading of the file this is; progress reports carry it.
    pub generation: u64,
    /// How the file is read.
    pub interpretation: Interpretation,
    /// The first rows, each as its cells.
    pub rows: Vec<Vec<Cell>>,
    /// Rows known so far.
    pub row_count: u64,
    /// The row count to size the scrollbar with.
    pub estimated_row_count: u64,
    /// The most common field count in the first 64 KB: the grid's column
    /// count until [`Document::column_count`] has the index's.
    pub column_count: u32,
}

/// Where indexing has got to. See [`leal_core::document::IndexProgress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct IndexProgress {
    /// The reading this is about.
    pub generation: u64,
    /// Rows that can be read now.
    pub rows: u64,
    /// The row count to size the scrollbar with: exact once `complete`.
    pub estimated_rows: u64,
    /// How far the index has got, in bytes.
    pub bytes_scanned: u64,
    /// The file's length.
    pub bytes_total: u64,
    /// Whether every row is indexed.
    pub complete: bool,
}

/// What the whole-file review suggests (ADR-0005 decision 4). The app
/// shows the suggestions; nothing changes by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ReviewResult {
    /// An encoding the whole file fits better: "Reopen as …".
    pub encoding_suggestion: Option<TextEncoding>,
    /// A delimiter the whole file fits better: "This file looks
    /// semicolon-separated — Switch".
    pub delimiter_suggestion: Option<Delimiter>,
    /// The most common line ending in the whole file.
    pub line_ending: Option<LineEnding>,
}

/// A kind of irregularity (DESIGN §3.5). See
/// [`leal_core::diagnostics::DiagnosticKind`] for what one occurrence of
/// each is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DiagnosticKind {
    /// A quote opened and never closed (error).
    UnterminatedQuote,
    /// A row with a different field count to most rows (warning).
    RaggedRows,
    /// `"a"b` (warning).
    TextAfterClosingQuote,
    /// Text that doesn't decode and shows as U+FFFD (warning).
    InvalidEncoding,
    /// NUL bytes, or U+0000 in UTF-16 (warning).
    NulBytes,
    /// A row whose line ending isn't the most common one (info).
    MixedLineEndings,
    /// An empty row (info).
    BlankLines,
    /// The file starts with a BOM (info).
    BomPresent,
}

impl From<leal_core::diagnostics::DiagnosticKind> for DiagnosticKind {
    fn from(kind: leal_core::diagnostics::DiagnosticKind) -> Self {
        use leal_core::diagnostics::DiagnosticKind as Core;
        match kind {
            Core::UnterminatedQuote => DiagnosticKind::UnterminatedQuote,
            Core::RaggedRows => DiagnosticKind::RaggedRows,
            Core::TextAfterClosingQuote => DiagnosticKind::TextAfterClosingQuote,
            Core::InvalidEncoding => DiagnosticKind::InvalidEncoding,
            Core::NulBytes => DiagnosticKind::NulBytes,
            Core::MixedLineEndings => DiagnosticKind::MixedLineEndings,
            Core::BlankLines => DiagnosticKind::BlankLines,
            Core::BomPresent => DiagnosticKind::BomPresent,
        }
    }
}

/// How serious a diagnostic is: warnings and errors show the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Severity {
    /// Status bar and details only.
    Info,
    /// Shows the banner.
    Warning,
    /// Shows the banner, prominently.
    Error,
}

impl From<leal_core::diagnostics::Severity> for Severity {
    fn from(severity: leal_core::diagnostics::Severity) -> Self {
        use leal_core::diagnostics::Severity as Core;
        match severity {
            Core::Info => Severity::Info,
            Core::Warning => Severity::Warning,
            Core::Error => Severity::Error,
        }
    }
}

/// Where one occurrence is: a row, and a byte offset into the file as
/// stored (ADR-0003 decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct DiagnosticLocation {
    /// The 0-based physical row.
    pub row: u64,
    /// The byte offset into the file, BOM included.
    pub offset: u64,
}

/// One kind of irregularity found in the file.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Diagnostic {
    /// What was found.
    pub kind: DiagnosticKind,
    /// How serious it is.
    pub severity: Severity,
    /// How many occurrences, in every row the report covers.
    pub count: u64,
    /// The first occurrences in file order, at most 1,000, for **Previous**
    /// and **Next** in the details popover.
    pub first: Vec<DiagnosticLocation>,
}

/// The diagnostics of the rows indexed so far. See
/// [`leal_core::diagnostics::Report`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DiagnosticsReport {
    /// The reading these are for (as in [`FirstScreen::generation`]).
    pub generation: u64,
    /// The rows the report covers.
    pub rows: u64,
    /// Whether the whole file is indexed. Until then, ragged rows and mixed
    /// line endings are relative to the rows so far.
    pub complete: bool,
    /// Every kind found, errors first.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether there is a warning or an error, which shows the banner.
    pub shows_banner: bool,
    /// How many kinds are warnings or errors: "This file has N kinds of
    /// irregularity".
    pub banner_kinds: u32,
    /// How many rows have the most common field count: "3 rows have a
    /// different number of fields to the other 1,245" (mockup 03b).
    pub rows_with_common_field_count: u64,
}

/// Where an occurrence of a diagnostic is: the cell the details popover's
/// **Previous** and **Next** select (task 1.7). See
/// [`leal_core::document::Place`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct DiagnosticPlace {
    /// The 0-based physical row (the header row, if any, is row 0).
    pub row: u64,
    /// The 0-based field.
    pub column: u32,
}

/// One row's marks: its gutter marker, and whether its missing cells are
/// hatched. See [`leal_core::diagnostics::RowFlags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct RowFlags {
    /// The row has a warning or an error.
    pub marked: bool,
    /// The row is ragged.
    pub ragged: bool,
}

impl From<document::Place> for DiagnosticPlace {
    fn from(place: document::Place) -> Self {
        DiagnosticPlace {
            row: to_u64(place.row),
            column: to_u32(place.column),
        }
    }
}

impl From<DiagnosticKind> for leal_core::diagnostics::DiagnosticKind {
    fn from(kind: DiagnosticKind) -> Self {
        use leal_core::diagnostics::DiagnosticKind as Core;
        match kind {
            DiagnosticKind::UnterminatedQuote => Core::UnterminatedQuote,
            DiagnosticKind::RaggedRows => Core::RaggedRows,
            DiagnosticKind::TextAfterClosingQuote => Core::TextAfterClosingQuote,
            DiagnosticKind::InvalidEncoding => Core::InvalidEncoding,
            DiagnosticKind::NulBytes => Core::NulBytes,
            DiagnosticKind::MixedLineEndings => Core::MixedLineEndings,
            DiagnosticKind::BlankLines => Core::BlankLines,
            DiagnosticKind::BomPresent => Core::BomPresent,
        }
    }
}

impl DiagnosticsReport {
    fn new(generation: u64, report: &leal_core::diagnostics::Report) -> Self {
        DiagnosticsReport {
            generation,
            rows: to_u64(report.rows()),
            complete: report.is_complete(),
            diagnostics: report
                .diagnostics()
                .iter()
                .map(|d| Diagnostic {
                    kind: d.kind().into(),
                    severity: d.severity().into(),
                    count: to_u64(d.count()),
                    first: d
                        .first()
                        .iter()
                        .map(|l| DiagnosticLocation {
                            row: to_u64(l.row),
                            offset: to_u64(l.offset),
                        })
                        .collect(),
                })
                .collect(),
            shows_banner: report.shows_banner(),
            banner_kinds: u32::try_from(
                report.kinds_at_least(leal_core::diagnostics::Severity::Warning),
            )
            .unwrap_or(u32::MAX),
            rows_with_common_field_count: to_u64(report.rows_with_common_field_count()),
        }
    }
}

/// Why a job didn't finish.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum JobFailure {
    /// It was cancelled.
    Cancelled,
    /// The file's removable drive was disconnected before it was read.
    DriveDisconnected,
    /// The file changed while it was read without a snapshot.
    ChangedOnDisk,
    /// The file on a network share was deleted by another computer before
    /// it was read (ADR-0009).
    DeletedElsewhere,
    /// The job's work panicked. DESIGN §3.9: the document is then treated
    /// as failed ([`Document::is_failed`]); the app shows an error and
    /// offers to reopen the file.
    Panicked {
        /// The panic's message. English, for logs.
        message: String,
    },
    /// Anything else. English, for logs.
    Failed {
        /// What went wrong.
        message: String,
    },
}

impl std::fmt::Display for JobFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::DriveDisconnected => f.write_str("the drive was disconnected"),
            Self::ChangedOnDisk => f.write_str("the file changed on disk"),
            Self::DeletedElsewhere => f.write_str("the file was deleted on its network share"),
            Self::Panicked { message } => write!(f, "the job panicked: {message}"),
            Self::Failed { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for JobFailure {}

impl From<JobError> for JobFailure {
    fn from(error: JobError) -> Self {
        match error {
            JobError::Cancelled => Self::Cancelled,
            JobError::Read(ReadErrorKind::Disconnected) => Self::DriveDisconnected,
            JobError::Read(ReadErrorKind::ChangedOnDisk) => Self::ChangedOnDisk,
            JobError::Read(ReadErrorKind::Deleted) => Self::DeletedElsewhere,
            JobError::Panicked(message) => Self::Panicked { message },
            other => Self::Failed {
                message: other.to_string(),
            },
        }
    }
}

/// A background job (ADR-0005 decision 6).
#[derive(Debug, uniffi::Object)]
pub struct Job {
    control: JobControl,
}

#[uniffi::export]
impl Job {
    /// Stops the job within one chunk of work. Swift calls it from
    /// `withTaskCancellationHandler` (task 1.6).
    pub fn cancel(&self) {
        self.control.cancel();
    }

    /// Whether the job has finished, however it finished.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.control.is_finished()
    }

    /// The job's id, which is also its `os_signpost` interval id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.control.id()
    }

    /// Waits for the job to finish, without blocking a thread: the job
    /// wakes the awaiting task when it ends.
    ///
    /// # Errors
    ///
    /// The [`JobFailure`] the job ended with.
    pub async fn wait(&self) -> Result<(), JobFailure> {
        Finished::new(self.control.clone())
            .await
            .map_err(JobFailure::from)
    }
}

/// A future that is ready when a job has finished. It registers the
/// awaiting task's [`Waker`] with the job, which wakes it from the job's
/// thread ([`JobControl::on_finish`]). No async runtime is needed: UniFFI
/// polls it from Swift.
struct Finished {
    control: JobControl,
    /// Set once the waker callback has been registered with the job.
    waker: Arc<Mutex<Option<Waker>>>,
    registered: bool,
}

impl Finished {
    fn new(control: JobControl) -> Self {
        Finished {
            control,
            waker: Arc::new(Mutex::new(None)),
            registered: false,
        }
    }
}

impl Future for Finished {
    type Output = Result<(), JobError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        // Store the newest waker before looking at the outcome, so a job
        // that finishes between the two still wakes this task.
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(context.waker().clone());
        if !self.registered {
            self.registered = true;
            let waker = Arc::clone(&self.waker);
            self.control.on_finish(move || {
                if let Some(waker) = waker.lock().unwrap_or_else(PoisonError::into_inner).take() {
                    waker.wake();
                }
            });
        }
        match self.control.outcome() {
            Some(outcome) => Poll::Ready(outcome),
            None => Poll::Pending,
        }
    }
}

/// An open document. See [`leal_core::document::Document`]. Releasing the
/// last reference cancels its jobs.
///
/// **After a panic the document has failed** (DESIGN §3.9): a panic in one
/// of its calls, or in one of its background jobs, may have left a lock
/// inside it poisoned or its state half-changed. From then on every call
/// returns [`LealError::DocumentFailed`] without touching the core
/// document; [`is_failed`](Self::is_failed) says so. The app shows an
/// error and offers to reopen the file.
#[derive(Debug, uniffi::Object)]
pub struct Document {
    /// Shared with its save jobs, which hold it while they run.
    document: Arc<document::Document>,
    /// The path, for errors.
    path: String,
    /// The latest first screen.
    first_screen: Mutex<FirstScreen>,
    /// Why the document failed, once it has.
    failure: Arc<Failure>,
}

/// Why a document failed: the first panic's message.
#[derive(Debug, Default)]
struct Failure {
    message: Mutex<Option<String>>,
}

impl Failure {
    /// Records a failure; only the first is kept.
    fn set(&self, message: String) {
        self.message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(message);
    }

    fn get(&self) -> Option<String> {
        self.message
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Marks the document failed if `job` ends in a panic.
    fn watch(self: &Arc<Self>, job: &JobControl) {
        let failure = Arc::clone(self);
        // A clone of the job's handle, inside the job's own callback list:
        // the list is emptied when the job finishes, which ends the cycle.
        let finished = job.clone();
        job.on_finish(move || {
            if let Some(Err(JobError::Panicked(message))) = finished.outcome() {
                failure.set(format!("a background job panicked: {message}"));
            }
        });
    }
}

/// Opens the file at `path` and reads its first screen (P0), then starts
/// indexing (P1) and the review (P2) in the background, on `scheduler`.
/// It returns once the first screen is ready (well under 150 ms, however
/// large the file), without waiting for the index. Call it off the main
/// thread if the file may be on a slow drive, and always if it may be on a
/// network share (ADR-0009): first paint reads the share, which can block,
/// and a debug build panics if that happens on the main thread.
///
/// `observer`, if given, is told about indexing progress.
///
/// # Errors
///
/// The open errors of [`crate::open_source`], and
/// [`LealError::EncodingDoesNotFit`], [`LealError::TooLarge`],
/// [`LealError::DriveDisconnected`] or [`LealError::ChangedOnDisk`].
#[uniffi::export]
pub fn open_document(
    path: &str,
    volume: VolumeInfo,
    temp: TempLocations,
    scheduler: &Scheduler,
    options: OpenOptions,
    observer: Option<Arc<dyn ProgressObserver>>,
) -> Result<Arc<Document>, LealError> {
    let temp = TempFolders::from(temp);
    let progress = observer.map(|observer| -> document::ProgressCallback {
        Arc::new(move |progress| observer.index_progressed(progress.into()))
    });
    let (document, screen) = document::Document::open(
        Path::new(path),
        &temp,
        volume.into(),
        &scheduler.scheduler,
        options.into(),
        progress,
    )
    .map_err(|error| document_error(path, error))?;
    let document = Document {
        document: Arc::new(document),
        path: path.to_owned(),
        first_screen: Mutex::new(screen.into()),
        failure: Arc::default(),
    };
    document.watch_jobs();
    Ok(Arc::new(document))
}

impl Document {
    /// Runs `call` unless the document has failed, and marks it failed if
    /// `call` panics. Every export of `Document` goes through this.
    fn call<T>(&self, call: impl FnOnce() -> Result<T, LealError>) -> Result<T, LealError> {
        guarded(&self.failure, &self.path, call)
    }

    /// A kind search's answer for Swift. A search stopped because a newer
    /// one started gives `None`: its answer isn't wanted.
    fn search_result(
        &self,
        found: Result<Option<document::Place>, ReadError>,
    ) -> Result<Option<DiagnosticPlace>, LealError> {
        match found {
            Ok(place) => Ok(place.map(DiagnosticPlace::from)),
            Err(error) if error.kind() == ReadErrorKind::Cancelled => Ok(None),
            Err(error) => Err(read_error(&self.path, &error)),
        }
    }

    /// After the core read the file again (a drive back): the cached first
    /// screen, which is the same, takes the new generation if it is the
    /// reading restarted (not one a save has replaced meanwhile), and the
    /// new jobs are watched.
    fn adopt_restart(&self, restarted: leal_core::document::Restarted) {
        {
            let mut cached = self
                .first_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if cached.generation == restarted.from {
                cached.generation = restarted.to;
            }
        }
        self.watch_jobs();
    }

    /// Marks the document failed if a job of its current reading panics.
    fn watch_jobs(&self) {
        self.failure.watch(self.document.index_job().control());
        self.failure.watch(self.document.review_job().control());
    }
}

/// Runs `call` unless the document (whose failure is `failure`, and whose
/// file is `path`) has failed, and marks it failed if `call` panics. Every
/// export of `Document`, `Search` and `CopyJob` goes through this.
fn guarded<T>(
    failure: &Failure,
    path: &str,
    call: impl FnOnce() -> Result<T, LealError>,
) -> Result<T, LealError> {
    let failed = |message| LealError::DocumentFailed {
        path: path.to_owned(),
        message,
    };
    if let Some(message) = failure.get() {
        return Err(failed(message));
    }
    // `AssertUnwindSafe`: after a panic nothing in the document is looked
    // at again, because it is marked failed first.
    match panic::catch_unwind(AssertUnwindSafe(call)) {
        Ok(result) => result,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            failure.set(message.clone());
            Err(failed(message))
        }
    }
}

/// The text of a panic, if it had one.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

#[uniffi::export]
impl Document {
    /// Whether the document has failed after a panic (see [`Document`]).
    /// Every other call then returns [`LealError::DocumentFailed`].
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failure.get().is_some()
    }

    /// The first screen of rows: the one made at opening, or by the latest
    /// [`reinterpret`](Self::reinterpret), cached as it was then. It
    /// doesn't follow edits made since; read rows with
    /// [`cells`](Self::cells) for the cells as they are now.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn first_screen(&self) -> Result<FirstScreen, LealError> {
        self.call(|| {
            Ok(self
                .first_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone())
        })
    }

    /// Rows `start` to `start + count` (as many as can be read now), with
    /// at most `max_chars` characters of each cell. Fast enough for the
    /// main thread.
    ///
    /// # Errors
    ///
    /// [`LealError::DriveDisconnected`], [`LealError::ChangedOnDisk`] or
    /// [`LealError::Io`], only for a file on a removable drive whose copy
    /// isn't complete; [`LealError::DocumentFailed`].
    pub fn rows(
        &self,
        start: u64,
        count: u32,
        max_chars: u32,
    ) -> Result<Vec<Vec<Cell>>, LealError> {
        self.call(|| {
            let rows = self
                .document
                .rows(to_range(start, count), to_usize(max_chars))
                .map_err(|error| read_error(&self.path, &error))?;
            Ok(rows
                .into_iter()
                .map(|row| row.into_iter().map(Cell::from).collect())
                .collect())
        })
    }

    /// Rows `row_start` to `row_start + row_count`, as for
    /// [`rows`](Self::rows), but only their cells in columns `column_start`
    /// to `column_start + column_count`, with each row's field count. The
    /// grid reads what it shows this way.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn cells(
        &self,
        row_start: u64,
        row_count: u32,
        column_start: u32,
        column_count: u32,
        max_chars: u32,
    ) -> Result<Vec<RowCells>, LealError> {
        self.call(|| {
            let rows = to_range(row_start, row_count);
            let columns = to_range(u64::from(column_start), column_count);
            let rows = self
                .document
                .cells(rows, columns, to_usize(max_chars))
                .map_err(|error| read_error(&self.path, &error))?;
            Ok(rows.into_iter().map(RowCells::from).collect())
        })
    }

    /// The grid's column count: the most common field count so far. It can
    /// change while indexing.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn column_count(&self) -> Result<u32, LealError> {
        self.call(|| Ok(to_u32(self.document.column_count())))
    }

    /// Which columns hold numbers, from the first `sample` rows after the
    /// header row, for right-aligning them. The first screen's sample is
    /// fast; call it with 1,000 rows off the main thread.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn numeric_columns(&self, sample: u32) -> Result<Vec<bool>, LealError> {
        self.call(|| {
            self.document
                .numeric_columns(to_usize(sample))
                .map_err(|error| read_error(&self.path, &error))
        })
    }

    /// Rows that can be read now; the file's row count once indexing is
    /// complete.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn row_count(&self) -> Result<u64, LealError> {
        self.call(|| Ok(to_u64(self.document.row_count())))
    }

    /// The row count to size the scrollbar with while indexing.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn estimated_row_count(&self) -> Result<u64, LealError> {
        self.call(|| Ok(to_u64(self.document.estimated_row_count())))
    }

    /// Where indexing has got to.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn progress(&self) -> Result<IndexProgress, LealError> {
        self.call(|| Ok(self.document.progress().into()))
    }

    /// How the file is read now.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn interpretation(&self) -> Result<Interpretation, LealError> {
        self.call(|| Ok((&self.document.detection()).into()))
    }

    /// Where the file's bytes are held.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn storage(&self) -> Result<SourceStorage, LealError> {
        self.call(|| Ok(self.document.storage().into()))
    }

    /// How many bytes, from the start of the file, can be read: all of them,
    /// except after a disconnection or a deletion before the copy was
    /// complete, when it is what was copied. A share that fails at the same
    /// place each time it reconnects is a bad read, not a share coming and
    /// going (task 2.0).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn available_bytes(&self) -> Result<u64, LealError> {
        self.call(|| Ok(self.document.source().available_len()))
    }

    /// Whether the file is on a network share (ADR-0009). The app then
    /// reloads it off the main thread, because opening it reads the share.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn is_on_network_share(&self) -> Result<bool, LealError> {
        self.call(|| Ok(self.document.source().is_on_network_share()))
    }

    /// The index job (P1) of the current reading.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn index_job(&self) -> Result<Arc<Job>, LealError> {
        self.call(|| {
            Ok(Arc::new(Job {
                control: self.document.index_job().control().clone(),
            }))
        })
    }

    /// The review job (P2) of the current reading.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn review_job(&self) -> Result<Arc<Job>, LealError> {
        self.call(|| {
            Ok(Arc::new(Job {
                control: self.document.review_job().control().clone(),
            }))
        })
    }

    /// What the review suggests, once it has finished; `None` before, or
    /// if it didn't finish.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn review(&self) -> Result<Option<ReviewResult>, LealError> {
        self.call(|| {
            let job = self.document.review_job();
            let Some(Ok(review)) = job.result() else {
                return Ok(None);
            };
            Ok(Some(ReviewResult {
                encoding_suggestion: review.encoding_suggestion.map(TextEncoding::from),
                delimiter_suggestion: review.delimiter_suggestion.map(Delimiter::from),
                line_ending: review.line_ending.map(LineEnding::from),
            }))
        })
    }

    /// What the index has found wrong with the file so far (DESIGN §3.5),
    /// for the banner, the details popover and the status bar. Complete
    /// once indexing is. It copies up to 1,000 locations per kind, so call
    /// it when progress is reported, not for every frame.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn diagnostics(&self) -> Result<DiagnosticsReport, LealError> {
        self.call(|| {
            let (generation, report) = self.document.diagnostics_with_generation();
            Ok(DiagnosticsReport::new(generation, &report))
        })
    }

    /// Whether row `row` has a warning or an error: its gutter marker.
    /// Every such row, not only the report's first locations. Fast enough
    /// to ask for each visible row.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn row_has_diagnostic(&self, row: u64) -> Result<bool, LealError> {
        self.call(|| Ok(self.document.row_has_diagnostic(to_index(row))))
    }

    /// The first row at or after `from` with a warning or an error, for
    /// **Next**.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn next_row_with_diagnostic(&self, from: u64) -> Result<Option<u64>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .next_row_with_diagnostic(to_index(from))
                .map(to_u64))
        })
    }

    /// The last row before `to` with a warning or an error, for
    /// **Previous**.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn previous_row_with_diagnostic(&self, to: u64) -> Result<Option<u64>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .previous_row_with_diagnostic(to_index(to))
                .map(to_u64))
        })
    }

    /// The first occurrence of `kind` in a row at or after `from`, for that
    /// kind's **Next** in the details popover, past the report's first
    /// 1,000 too. `None` for the info-level kinds and after the last one.
    /// It may read many rows: call it off the main thread. Starting another
    /// search stops this one, which then gives `None`.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn next_with_kind(
        &self,
        kind: DiagnosticKind,
        from: u64,
    ) -> Result<Option<DiagnosticPlace>, LealError> {
        self.call(|| {
            let found = self.document.next_with_kind(kind.into(), to_index(from));
            self.search_result(found)
        })
    }

    /// The last occurrence of `kind` in a row before `to`, for
    /// **Previous**. As for [`next_with_kind`](Self::next_with_kind).
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn previous_with_kind(
        &self,
        kind: DiagnosticKind,
        to: u64,
    ) -> Result<Option<DiagnosticPlace>, LealError> {
        self.call(|| {
            let found = self.document.previous_with_kind(kind.into(), to_index(to));
            self.search_result(found)
        })
    }

    /// The marks of rows `start` to `start + count`: their gutter markers,
    /// and which are ragged. One call per screenful.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn row_flags(&self, start: u64, count: u32) -> Result<Vec<RowFlags>, LealError> {
        self.call(|| {
            Ok(self
                .document
                .row_flags(to_range(start, count))
                .into_iter()
                .map(|flags| RowFlags {
                    marked: flags.marked,
                    ragged: flags.ragged,
                })
                .collect())
        })
    }

    /// Whether Save (writing over the original) is possible: not after the
    /// file's removable drive was disconnected, or the file changed while
    /// it was read (ADR-0006). Save As always is.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn can_save(&self) -> Result<bool, LealError> {
        self.call(|| Ok(self.document.can_save()))
    }

    /// Whether the file changed on its drive while Leal was reading it, so
    /// the rows shown may mix two versions (the 1.1a "changed while
    /// loading" state).
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn changed_on_disk(&self) -> Result<bool, LealError> {
        self.call(|| Ok(self.document.changed_on_disk()))
    }

    /// The user's file as last seen (task 1.9). No system calls.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn original(&self) -> Result<OriginalStatus, LealError> {
        self.call(|| Ok(self.document.original().into()))
    }

    /// Starts watching the user's file (task 1.9): `observer` is told each
    /// time what has happened to it changes. A second call does nothing.
    ///
    /// # Errors
    ///
    /// [`LealError::Internal`] if the kernel's event queue or the watching
    /// thread can't be made; the document works without them, and
    /// [`check_original`](Self::check_original) still notices changes.
    /// [`LealError::DocumentFailed`].
    pub fn watch_original(&self, observer: Arc<dyn OriginalObserver>) -> Result<(), LealError> {
        self.call(|| {
            self.document
                .watch_original(Arc::new(move |status| {
                    observer.original_changed(status.clone().into());
                }))
                .map_err(|error| LealError::Internal {
                    message: format!("couldn't watch {}: {error}", self.path),
                })
        })
    }

    /// Looks at the user's file now (task 1.9): the app calls it when a
    /// volume mounts or the app becomes active. If the file's removable
    /// drive is back and the file is unchanged, a disconnected document
    /// reconnects and is read again, with a new generation (its jobs are
    /// new: [`index_job`](Self::index_job)), to carry on copying; Save is
    /// allowed again. It makes a few system calls, which a network volume
    /// can slow down: call it off the main thread.
    ///
    /// # Errors
    ///
    /// [`LealError::DocumentFailed`].
    pub fn check_original(&self) -> Result<OriginalStatus, LealError> {
        self.call(|| {
            let (status, restarted) = self.document.check_original_restarting();
            if let Some(restarted) = restarted {
                self.adopt_restart(restarted);
            }
            Ok(status.into())
        })
    }

    /// Reads the file again with `options`' choices (**Treat as**, the
    /// header toggle, **Reopen with encoding…**), without reopening it.
    /// The old jobs are cancelled; [`index_job`](Self::index_job) and
    /// [`review_job`](Self::review_job) give the new ones.
    ///
    /// While the document has unsaved edits, only the header row choice
    /// can change, and the edits are kept (ADR-0008 decision 4).
    ///
    /// # Errors
    ///
    /// [`LealError::EncodingDoesNotFit`], or [`LealError::UnsavedEdits`]
    /// for another delimiter or encoding while there are edits; the
    /// document is then unchanged. [`LealError::DocumentFailed`].
    pub fn reinterpret(&self, options: OpenOptions) -> Result<FirstScreen, LealError> {
        self.call(|| {
            let options = document::OpenOptions::from(options);
            let screen: FirstScreen = self
                .document
                .reinterpret(
                    options.choices,
                    options.first_screen_rows,
                    options.max_chars,
                )
                .map_err(|error| document_error(&self.path, error))?
                .into();
            self.watch_jobs();
            *self
                .first_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = screen.clone();
            Ok(screen)
        })
    }
}

/// Exports only the app's tests call (the `test-exports` feature, as for
/// `debug_panic`): ways to make a document fail.
#[cfg(feature = "test-exports")]
#[uniffi::export]
impl Document {
    /// Panics inside a document call, which marks the document failed.
    ///
    /// # Errors
    ///
    /// Always [`LealError::DocumentFailed`].
    ///
    /// # Panics
    ///
    /// Inside the call, on purpose; the panic is caught there.
    pub fn debug_panic(&self) -> Result<(), LealError> {
        self.call(|| panic!("deliberate document panic"))
    }

    /// Treats `job` as one of this document's jobs: if it panics, the
    /// document fails.
    pub fn debug_watch(&self, job: &Job) {
        self.failure.watch(&job.control);
    }

    /// For a document from [`debug_open_document_with_fault`]: its
    /// simulated drive is plugged back in (task 1.9). The next
    /// [`check_original`](Self::check_original) reconnects it, as for a
    /// real drive. For one from [`debug_open_document_simulating_share`]:
    /// its share answers again.
    pub fn debug_simulate_drive_back(&self) {
        self.document.source().simulate_drive_back();
    }

    /// How many times the document's network share was read on the main
    /// thread, which ADR-0009 forbids (task 2.0). The app's tests check
    /// that it stays 0.
    #[must_use]
    pub fn debug_share_reads_on_main_thread(&self) -> u64 {
        to_u64(self.document.source().share_reads_on_main_thread())
    }

    /// For a document from [`debug_open_document_simulating_share`] with
    /// `hold_at`: the copy's reads held there go on.
    pub fn debug_share_release(&self) {
        self.document.source().simulated_share_release();
    }

    /// For a document from [`debug_open_document_holding_copy`]: the copy's
    /// reads held at `hold_at` go on.
    pub fn debug_release_held_copy(&self) {
        self.document.source().release_held_copy();
    }
}

/// Reads of a simulated network share that fail, for the app's tests
/// (`test-exports`). See `leal_core::source::SimulatedShareFailure`.
#[cfg(feature = "test-exports")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SimulatedShareFailure {
    /// Reads of bytes from this one on fail.
    pub at: u64,
    /// With this error code (an errno, such as `ETIMEDOUT` or `ESTALE`).
    pub errno: i32,
    /// This many times, then they work again; `nil`: until
    /// [`Document::debug_simulate_drive_back`].
    pub times: Option<u32>,
    /// The `fstat` after the read fails, not the read.
    #[uniffi(default = false)]
    pub on_stat: bool,
    /// The read leaves junk in half its buffer before failing.
    #[uniffi(default = false)]
    pub partial: bool,
}

/// The waits between retries of a simulated share's network errors: short,
/// so the app's tests needn't wait the real 3 s.
#[cfg(feature = "test-exports")]
const SIMULATED_RETRY_DELAYS: &[std::time::Duration] = &[std::time::Duration::from_millis(5); 5];

/// [`open_document`], as if the file were on a network share (ADR-0009,
/// task 2.0) that can't clone: copied in chunks of `chunk_bytes`, each read
/// of the share taking `read_delay_ms`, failing as `failure` says, and the
/// copy's reads past `hold_at` waiting for [`Document::debug_share_release`]
/// (so a test can look at the document part-way through). For
/// the app's tests of loading rows and of the share's banners
/// (`test-exports`). Call it off the main thread, as for a real share.
///
/// # Errors
///
/// As for [`open_document`].
#[cfg(feature = "test-exports")]
#[uniffi::export]
#[expect(
    clippy::too_many_arguments,
    reason = "a test export, mirroring open_document and the share's knobs"
)]
pub fn debug_open_document_simulating_share(
    path: &str,
    temp: TempLocations,
    scheduler: &Scheduler,
    options: OpenOptions,
    observer: Option<Arc<dyn ProgressObserver>>,
    chunk_bytes: u32,
    read_delay_ms: u32,
    failure: Option<SimulatedShareFailure>,
    hold_at: Option<u64>,
) -> Result<Arc<Document>, LealError> {
    use leal_core::source::{SimulatedShare, SimulatedShareFailure as Core, Source};
    let temp = TempFolders::from(temp);
    let share = SimulatedShare {
        read_delay: std::time::Duration::from_millis(u64::from(read_delay_ms)),
        failure: failure.map(|failure| Core {
            at: usize::try_from(failure.at).unwrap_or(usize::MAX),
            errno: failure.errno,
            times: failure.times,
            on_stat: failure.on_stat,
            partial: failure.partial,
        }),
        retry_delays: SIMULATED_RETRY_DELAYS,
        retry_window: None,
        hold_at: hold_at.map(|at| usize::try_from(at).unwrap_or(usize::MAX)),
        on_close: None,
    };
    let source =
        Source::open_simulating_share(Path::new(path), &temp, to_usize(chunk_bytes), share)
            .map_err(|error| LealError::from_open(path, &error))?;
    debug_document_from(path, source, scheduler, options, observer)
}

/// [`open_document`], the real thing, except that if the file is on a
/// removable drive (or a network share) the copy's reads past `hold_at`
/// wait for [`Document::debug_release_held_copy`], or for the document to
/// be closed. So the app's tests can pull a real drive (a disk image) while
/// the copy is certainly part-way through, however long the pull takes,
/// then let the copy go on and find the drive gone (`test-exports`). See
/// `leal_core::source::Source::open_holding_copy`.
///
/// # Errors
///
/// As for [`open_document`].
#[cfg(feature = "test-exports")]
#[uniffi::export]
pub fn debug_open_document_holding_copy(
    path: &str,
    volume: VolumeInfo,
    temp: TempLocations,
    scheduler: &Scheduler,
    options: OpenOptions,
    observer: Option<Arc<dyn ProgressObserver>>,
    hold_at: u64,
) -> Result<Arc<Document>, LealError> {
    let temp = TempFolders::from(temp);
    let hold_at = usize::try_from(hold_at).unwrap_or(usize::MAX);
    let source = leal_core::source::Source::open_holding_copy(
        Path::new(path),
        &temp,
        volume.into(),
        hold_at,
    )
    .map_err(|error| LealError::from_open(path, &error))?;
    debug_document_from(path, source, scheduler, options, observer)
}

/// A P2 job that panics, for the app's tests (`test-exports`).
#[cfg(feature = "test-exports")]
#[uniffi::export]
#[must_use]
pub fn debug_panicking_job(scheduler: &Scheduler) -> Arc<Job> {
    let handle: schedule::JobHandle<()> =
        scheduler
            .scheduler
            .spawn(schedule::Priority::P2, schedule::Interval::Review, |_| {
                panic!("deliberate job panic")
            });
    Arc::new(Job {
        control: handle.control().clone(),
    })
}

/// What to pretend happens to a file on a removable drive while it is
/// copied, for the app's tests (`test-exports`). See
/// `leal_core::source::SimulatedFault`.
#[cfg(feature = "test-exports")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SimulatedFault {
    /// The drive vanishes when the copy reaches byte `at`.
    Disconnect {
        /// The first byte that can't be read.
        at: u64,
    },
    /// The file changes when the copy reaches byte `at`.
    Change {
        /// The first byte read after the change.
        at: u64,
    },
}

/// [`open_document`], as if the file were on a removable drive, copied in
/// chunks of `chunk_bytes`, with `fault` happening during the copy: for the
/// app's tests of the "drive disconnected" and "changed while reading"
/// banners (task 1.7, `test-exports`).
///
/// # Errors
///
/// As for [`open_document`].
#[cfg(feature = "test-exports")]
#[uniffi::export]
pub fn debug_open_document_with_fault(
    path: &str,
    temp: TempLocations,
    scheduler: &Scheduler,
    options: OpenOptions,
    observer: Option<Arc<dyn ProgressObserver>>,
    chunk_bytes: u32,
    fault: Option<SimulatedFault>,
) -> Result<Arc<Document>, LealError> {
    use leal_core::source::{SimulatedFault as Core, Source};
    let temp = TempFolders::from(temp);
    let at = |at: u64| usize::try_from(at).unwrap_or(usize::MAX);
    let fault = fault.map(|fault| match fault {
        SimulatedFault::Disconnect { at: byte } => Core::Disconnect { at: at(byte) },
        SimulatedFault::Change { at: byte } => Core::Change { at: at(byte) },
    });
    let source =
        Source::open_simulating_fault(Path::new(path), &temp, to_usize(chunk_bytes), fault)
            .map_err(|error| LealError::from_open(path, &error))?;
    debug_document_from(path, source, scheduler, options, observer)
}

/// A document for the test exports, from a source they opened.
#[cfg(feature = "test-exports")]
fn debug_document_from(
    path: &str,
    source: leal_core::source::Source,
    scheduler: &Scheduler,
    options: OpenOptions,
    observer: Option<Arc<dyn ProgressObserver>>,
) -> Result<Arc<Document>, LealError> {
    let progress = observer.map(|observer| -> document::ProgressCallback {
        Arc::new(move |progress| observer.index_progressed(progress.into()))
    });
    let (document, screen) =
        document::Document::from_source(source, &scheduler.scheduler, options.into(), progress)
            .map_err(|error| document_error(path, error))?;
    let document = Document {
        document: Arc::new(document),
        path: path.to_owned(),
        first_screen: Mutex::new(screen.into()),
        failure: Arc::default(),
    };
    document.watch_jobs();
    Ok(Arc::new(document))
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `count` items from `start`, as a range of `usize`, saturating.
fn to_range(start: u64, count: u32) -> std::ops::Range<usize> {
    let start = usize::try_from(start).unwrap_or(usize::MAX);
    start..start.saturating_add(to_usize(count))
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A row number from Swift. One past every row is as good as any larger
/// number, so a value too large for `usize` (impossible on a 64-bit Mac)
/// saturates.
fn to_index(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn document_error(path: &str, error: DocumentError) -> LealError {
    let path_owned = path.to_owned();
    match error {
        DocumentError::Open(error) => LealError::from_open(path, &error),
        DocumentError::Read(error) => read_error(path, &error),
        DocumentError::Choice(_) => LealError::EncodingDoesNotFit { path: path_owned },
        DocumentError::TooLarge { len } => LealError::TooLarge {
            path: path_owned,
            byte_count: len,
        },
        DocumentError::UnsavedEdits => LealError::UnsavedEdits { path: path_owned },
        DocumentError::Saving => LealError::Saving { path: path_owned },
        DocumentError::Internal(message) => LealError::Internal { message },
    }
}

fn read_error(path: &str, error: &ReadError) -> LealError {
    let path = path.to_owned();
    match error.kind() {
        ReadErrorKind::Disconnected => LealError::DriveDisconnected { path },
        ReadErrorKind::ChangedOnDisk => LealError::ChangedOnDisk { path },
        ReadErrorKind::Deleted => LealError::DeletedElsewhere { path },
        // `NotCopied` never reaches Swift: every row the core serves is in
        // the copy. If it did, it is an I/O error for the logs.
        ReadErrorKind::Cancelled | ReadErrorKind::NotCopied | ReadErrorKind::Other => {
            LealError::Io {
                path,
                code: error.raw_os_error(),
                message: error.to_string(),
            }
        }
    }
}

impl From<OpenOptions> for document::OpenOptions {
    fn from(options: OpenOptions) -> Self {
        document::OpenOptions {
            choices: Choices {
                delimiter: options.delimiter.map(dialect::Delimiter::from),
                header: options.header,
                encoding: options.encoding.map(dialect::Encoding::from),
            },
            first_screen_rows: to_usize(options.first_screen_rows),
            max_chars: to_usize(options.max_chars),
        }
    }
}

impl From<document::Cell> for Cell {
    fn from(cell: document::Cell) -> Self {
        Cell {
            text: cell.text,
            truncated: cell.truncated,
        }
    }
}

impl From<document::RowCells> for RowCells {
    fn from(row: document::RowCells) -> Self {
        RowCells {
            field_count: to_u32(row.field_count),
            cells: row.cells.into_iter().map(Cell::from).collect(),
        }
    }
}

impl From<document::FirstScreen> for FirstScreen {
    fn from(screen: document::FirstScreen) -> Self {
        FirstScreen {
            generation: screen.generation,
            interpretation: (&screen.detection).into(),
            rows: screen
                .rows
                .into_iter()
                .map(|row| row.into_iter().map(Cell::from).collect())
                .collect(),
            row_count: to_u64(screen.row_count),
            estimated_row_count: to_u64(screen.estimated_row_count),
            column_count: to_u32(screen.column_count),
        }
    }
}

impl From<document::IndexProgress> for IndexProgress {
    fn from(progress: document::IndexProgress) -> Self {
        IndexProgress {
            generation: progress.generation,
            rows: to_u64(progress.rows),
            estimated_rows: to_u64(progress.estimated_rows),
            bytes_scanned: progress.bytes_scanned,
            bytes_total: progress.bytes_total,
            complete: progress.complete,
        }
    }
}

impl From<&Detection> for Interpretation {
    fn from(detection: &Detection) -> Self {
        Interpretation {
            encoding: detection.encoding.into(),
            encoding_source: detection.encoding_source.into(),
            delimiter: detection.delimiter.into(),
            delimiter_source: detection.delimiter_source.into(),
            header: detection.header,
            header_source: detection.header_source.into(),
            line_ending: detection.line_ending.map(LineEnding::from),
            notes: detection
                .notes
                .iter()
                .copied()
                .map(InterpretationNote::from)
                .collect(),
            encoding_choices: detection
                .encoding_choices()
                .into_iter()
                .map(TextEncoding::from)
                .collect(),
        }
    }
}

impl From<detect::EncodingSource> for EncodingSource {
    fn from(source: detect::EncodingSource) -> Self {
        match source {
            detect::EncodingSource::Bom => Self::Bom,
            detect::EncodingSource::Attribute => Self::Attribute,
            detect::EncodingSource::Guess => Self::Guess,
            detect::EncodingSource::User => Self::User,
        }
    }
}

impl From<detect::DialectSource> for DialectSource {
    fn from(source: detect::DialectSource) -> Self {
        match source {
            detect::DialectSource::Attribute => Self::Attribute,
            detect::DialectSource::Guess => Self::Guess,
            detect::DialectSource::User => Self::User,
        }
    }
}

/// The two delimiter enums, one for each direction.
macro_rules! both_ways {
    ($ffi:ident, $core:path, [$($variant:ident),* $(,)?]) => {
        impl From<$core> for $ffi {
            fn from(value: $core) -> Self {
                match value {
                    $(<$core>::$variant => Self::$variant,)*
                }
            }
        }

        impl From<$ffi> for $core {
            fn from(value: $ffi) -> Self {
                match value {
                    $($ffi::$variant => <$core>::$variant,)*
                }
            }
        }
    };
}

both_ways!(Delimiter, dialect::Delimiter, [Comma, Semicolon, Tab, Pipe]);
both_ways!(LineEnding, dialect::LineEnding, [Lf, Crlf, Cr]);
both_ways!(
    TextEncoding,
    dialect::Encoding,
    [
        Utf8,
        Utf16Le,
        Utf16Be,
        Windows1252,
        Windows1250,
        Windows1251,
        Windows1253,
        Windows1254,
        Windows1255,
        Windows1256,
        Windows1257,
        Windows1258,
        Iso8859_1,
        Iso8859_2,
        Iso8859_15,
        MacRoman,
    ]
);

mod editing;
mod find;
mod saving;
#[cfg(test)]
mod tests;

pub use editing::{
    CellEdit, CellPlace, EditCommand, EditRefusal, RefusedCommand, ReplayReport,
    UnencodableCharacter, ValueChange,
};
pub use find::{CellMatch, CellValue, CopyJob, Search, SearchProgress, SearchStep, TextRange};
pub use saving::{
    SaveFailure, SaveJob, SaveKind, SaveOptions, SaveOutcome, SavePhase, SaveProgress,
};
