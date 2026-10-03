//! Diagnostics: the irregularities of messy input (DESIGN §3.5).
//!
//! Leal's rule for irregular input is **show it faithfully, warn clearly,
//! never fix it silently**. This module only describes: it reads the bytes
//! and never changes them.
//!
//! Diagnostics are collected during the row index's single pass (DESIGN
//! §3.3), not in a pass of their own:
//!
//! ```
//! use leal_core::diagnostics::{DiagnosticKind, Location, Severity};
//! use leal_core::index::{CodeUnit, IndexDialect, RowIndex};
//! use leal_core::dialect::Encoding;
//!
//! let bytes = b"id,name\n1,\"Jo\"x\n2\n";
//! let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
//! let (index, report) = RowIndex::build_with_diagnostics(bytes, dialect, Encoding::Utf8)?;
//!
//! assert!(report.is_complete());
//! let kinds: Vec<_> = report.diagnostics().iter().map(|d| d.kind()).collect();
//! assert_eq!(kinds, [DiagnosticKind::RaggedRows, DiagnosticKind::TextAfterClosingQuote]);
//!
//! let text = report.get(DiagnosticKind::TextAfterClosingQuote).unwrap();
//! assert_eq!(text.severity(), Severity::Warning);
//! assert_eq!(text.count(), 1);
//! assert_eq!(text.first(), [Location { row: 1, offset: 14 }]);
//! assert!(report.shows_banner());
//! # Ok::<(), leal_core::index::IndexError>(())
//! ```
//!
//! The app indexes on a background thread, and reads the diagnostics found
//! so far while it runs, with [`RowIndex::start_with_diagnostics`]:
//!
//! [`RowIndex::start_with_diagnostics`]: crate::index::RowIndex::start_with_diagnostics
//!
//! ```
//! use std::sync::atomic::AtomicBool;
//! use leal_core::index::{CodeUnit, IndexDialect, RowIndex};
//! use leal_core::dialect::Encoding;
//!
//! let bytes = b"a,b\n\n1,2\n";
//! let dialect = IndexDialect { delimiter: b',', quote: b'"', code_unit: CodeUnit::Byte, bom_len: 0 };
//! let (index, diagnostics, indexer) = RowIndex::start_with_diagnostics(dialect, Encoding::Utf8)?;
//! // (On the indexing thread.)
//! indexer.run(bytes, &AtomicBool::new(false), |progress| {
//!     // Tell the UI that rows and diagnostics changed. A reader calls
//!     // `diagnostics.report()` whenever it likes.
//!     let _ = progress.rows;
//! })?;
//! let report = diagnostics.report();
//! assert!(report.is_complete());
//! assert_eq!(report.rows(), index.row_count());
//! assert_eq!(report.diagnostics().len(), 1); // one blank line
//! # Ok::<(), leal_core::index::IndexError>(())
//! ```
//!
//! # What counts as one occurrence
//!
//! ADR-0003 decisions 4, 5 and 7, as the testkit and the corpus sidecars
//! (`tests/corpus/README.md`) pin them. Each kind's docs say what one
//! occurrence is and which byte its location points at. In short:
//!
//! - per **row**: ragged rows, blank lines, mixed line endings;
//! - per **field**: text after a closing quote, invalid encoding, NUL;
//! - per **file**: unterminated quote, BOM.
//!
//! Every location is a 0-based physical row and a byte offset into the file
//! as stored, the BOM included, in UTF-16 too (ADR-0003 decision 6). Each
//! diagnostic keeps its count and its first [`MAX_LOCATIONS`] locations in
//! file order (DESIGN §3.5).
//!
//! # While indexing
//!
//! A [`Report`] read while the indexer runs describes exactly the rows
//! indexed so far ([`Report::rows`]): it is what a complete index of just
//! those rows would report. So it can change as more rows arrive:
//!
//! - the dominant field count, and with it which rows are **ragged**, is the
//!   most common count *so far*; the same goes for the dominant line ending
//!   and **mixed line endings**. They are final once the report
//!   [is complete](Report::is_complete);
//! - an **unterminated quote** can only be known at the end of the file;
//! - the other kinds only ever gain occurrences.
//!
//! # Not in this module
//!
//! The whole-file encoding count (ADR-0003 decision 1) is detection's P2
//! work (ADR-0005 decision 4). If it suggests another encoding and the user
//! accepts, the file is re-indexed with it, which gives new diagnostics.

mod collect;
mod find;
mod marks;
#[cfg(test)]
mod tests;

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use crate::dialect::{Encoding, LineEnding};
use crate::index::{IndexDialect, IndexError};

pub(crate) use collect::Collector;
pub(crate) use find::{
    Hit, decided_by_bytes, field_has, field_with, has_invalid, next_hit, row_may_have, value_has,
};
use marks::RowMarks;
pub(crate) use marks::{Mark, RowCode};

#[cfg(test)]
thread_local! {
    /// How many times this thread searched the row marks for the next or
    /// previous marked row: a test hook for the cost of **Next** and
    /// **Previous** over inserted and deleted rows (task 2.4a).
    pub(crate) static MARK_SEARCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The most locations a diagnostic keeps (DESIGN §3.5). The count covers
/// every occurrence.
pub const MAX_LOCATIONS: usize = 1000;

/// A kind of irregularity (DESIGN §3.5). Each variant says what one
/// occurrence is and where its location points.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DiagnosticKind {
    /// Error. At most one: the quoted field whose opening quote never
    /// closes, so it runs to the end of the file. Location: the opening
    /// quote.
    UnterminatedQuote,
    /// Warning. One per non-blank row whose field count differs from the
    /// most common field count among non-blank rows (ties go to the count
    /// seen first). Blank lines are never ragged. Location: the row's start.
    RaggedRows,
    /// Warning. One per field with units between its closing quote and the
    /// next delimiter or line ending (`"a"b`). Location: the first of them.
    TextAfterClosingQuote,
    /// Warning. One per field containing text that doesn't decode, and so
    /// displays as U+FFFD: invalid UTF-8 (split into sequences the way
    /// `String::from_utf8_lossy` splits them), an unpaired UTF-16 surrogate
    /// or a final odd byte in a UTF-16 file, or a byte a single-byte
    /// encoding doesn't map. Location: the first invalid byte (in UTF-16,
    /// the first byte of the code unit).
    InvalidEncoding,
    /// Warning. One per field containing a NUL: a 0x00 byte, or in UTF-16 a
    /// U+0000 code unit (ADR-0003 decision 7). Location: the first NUL (its
    /// code unit's first byte).
    NulBytes,
    /// Info. One per row whose line ending differs from the most common one
    /// (ties go to the one seen first). Location: the first byte of the
    /// row's line ending.
    MixedLineEndings,
    /// Info. One per row with no bytes before its line ending, anywhere in
    /// the file, including at the end (ADR-0003 decision 5). Location: the
    /// row's start.
    BlankLines,
    /// Info. One if the file starts with a BOM. Location: row 0, offset 0.
    BomPresent,
}

impl DiagnosticKind {
    /// Every kind, errors first, in the order a [`Report`] lists them.
    pub const ALL: [DiagnosticKind; 8] = [
        DiagnosticKind::UnterminatedQuote,
        DiagnosticKind::RaggedRows,
        DiagnosticKind::TextAfterClosingQuote,
        DiagnosticKind::InvalidEncoding,
        DiagnosticKind::NulBytes,
        DiagnosticKind::MixedLineEndings,
        DiagnosticKind::BlankLines,
        DiagnosticKind::BomPresent,
    ];

    /// How serious this kind is (DESIGN §3.5).
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            DiagnosticKind::UnterminatedQuote => Severity::Error,
            DiagnosticKind::RaggedRows
            | DiagnosticKind::TextAfterClosingQuote
            | DiagnosticKind::InvalidEncoding
            | DiagnosticKind::NulBytes => Severity::Warning,
            DiagnosticKind::MixedLineEndings
            | DiagnosticKind::BlankLines
            | DiagnosticKind::BomPresent => Severity::Info,
        }
    }
}

/// How serious a diagnostic is. Warnings and errors show the banner;
/// info-level ones appear in the status bar and the details view only
/// (DESIGN §3.5, ADR-0002 question 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Status bar and details view only.
    Info,
    /// Shows the banner.
    Warning,
    /// Shows the banner, prominently.
    Error,
}

/// Where one occurrence is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    /// The 0-based physical row.
    pub row: usize,
    /// The byte offset into the file as stored, BOM included.
    pub offset: usize,
}

/// One kind of irregularity found in a file: how many times, and the first
/// places.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    kind: DiagnosticKind,
    count: usize,
    first: Vec<Location>,
}

impl Diagnostic {
    /// What was found.
    #[must_use]
    pub fn kind(&self) -> DiagnosticKind {
        self.kind
    }

    /// The kind's severity.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }

    /// How many occurrences there are: at least 1, and in every row the
    /// report covers, not only the ones in [`first`](Diagnostic::first).
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// The first occurrences in file order: all of them, up to
    /// [`MAX_LOCATIONS`].
    #[must_use]
    pub fn first(&self) -> &[Location] {
        &self.first
    }
}

/// The diagnostics of a file, or of the rows indexed so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    diagnostics: Vec<Diagnostic>,
    rows: usize,
    complete: bool,
    line_ending: Option<LineEnding>,
}

impl Report {
    /// Every kind found, in [`DiagnosticKind::ALL`] order. Kinds not found
    /// aren't listed.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// The diagnostic of one kind, if any was found.
    #[must_use]
    pub fn get(&self, kind: DiagnosticKind) -> Option<&Diagnostic> {
        self.diagnostics.iter().find(|d| d.kind == kind)
    }

    /// The most common line ending among the rows so far (a tie goes to
    /// the one seen first), or `None` if no row has one: what a save ends
    /// an inserted row with (ADR-0004 decision 3, task 2.4c).
    #[must_use]
    pub fn line_ending(&self) -> Option<LineEnding> {
        self.line_ending
    }

    /// How many rows this report covers: the rows indexed when it was made.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// True once the whole file is indexed. Until then the report covers
    /// only [`rows`](Report::rows) rows, and ragged rows and mixed line
    /// endings are relative to those rows (see the module docs).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// How many kinds have at least `severity`. With
    /// [`Severity::Warning`], that is the kinds the banner counts ("This
    /// file has N kinds of irregularity").
    #[must_use]
    pub fn kinds_at_least(&self, severity: Severity) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity() >= severity)
            .count()
    }

    /// True if there is a warning or an error, which shows the banner
    /// (DESIGN §3.5).
    #[must_use]
    pub fn shows_banner(&self) -> bool {
        self.kinds_at_least(Severity::Warning) > 0
    }

    /// How many rows have the most common field count: the rows that are
    /// neither blank nor ragged. The details popover says "3 rows have a
    /// different number of fields to the other 1,245" (mockup 03b).
    #[must_use]
    pub fn rows_with_common_field_count(&self) -> usize {
        let count = |kind| self.get(kind).map_or(0, Diagnostic::count);
        self.rows
            .saturating_sub(count(DiagnosticKind::BlankLines))
            .saturating_sub(count(DiagnosticKind::RaggedRows))
    }
}

/// One row's marks (task 1.7): what its gutter shows, and whether its
/// missing cells are hatched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowFlags {
    /// The row has a warning or an error: its gutter marker
    /// ([`Diagnostics::row_has_diagnostic`]).
    pub marked: bool,
    /// The row is ragged ([`Diagnostics::row_is_ragged`]).
    pub ragged: bool,
}

/// The diagnostics of a file being indexed, shared between the indexer and
/// its readers. Made by [`RowIndex::start_with_diagnostics`], or by
/// [`Indexer::with_diagnostics`] on the index's own thread; the
/// [`Indexer`](crate::index::Indexer) publishes a new [`Report`] and the
/// new rows' marks after each chunk, before its progress callback.
///
/// [`RowIndex::start_with_diagnostics`]: crate::index::RowIndex::start_with_diagnostics
/// [`Indexer::with_diagnostics`]: crate::index::Indexer::with_diagnostics
pub struct Diagnostics {
    dialect: IndexDialect,
    encoding: Encoding,
    shared: RwLock<Shared>,
}

/// What readers see, replaced or extended once per chunk.
#[derive(Debug, Default)]
struct Shared {
    report: Arc<Report>,
    marks: RowMarks,
}

impl fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Diagnostics")
            .field("dialect", &self.dialect)
            .field("encoding", &self.encoding)
            .finish_non_exhaustive()
    }
}

impl Diagnostics {
    /// Diagnostics for a file indexed with `dialect`, whose text is in
    /// `encoding`. The encoding must store characters the way the dialect
    /// says ([`Encoding::code_unit`]).
    pub(crate) fn new(dialect: IndexDialect, encoding: Encoding) -> Result<Self, IndexError> {
        if encoding.code_unit() != dialect.code_unit {
            return Err(IndexError::EncodingMismatch {
                code_unit: dialect.code_unit,
                encoding,
            });
        }
        Ok(Diagnostics {
            dialect,
            encoding,
            shared: RwLock::new(Shared::default()),
        })
    }

    /// The latest report: empty and incomplete until the indexer has
    /// scanned its first chunk, then the diagnostics of the rows indexed so
    /// far, and complete once the indexer finishes. If indexing stops
    /// early (cancelled or failed), the last report stays incomplete.
    ///
    /// It is shared (`Arc`), so reading it copies nothing.
    #[must_use]
    pub fn report(&self) -> Arc<Report> {
        Arc::clone(&self.read().report)
    }

    /// True if row `row` has a warning or an error: the rows the gutter
    /// marks (ADR-0002 question 7). Unlike a report's locations, which stop
    /// at [`MAX_LOCATIONS`] per kind, this covers every row. Info-level
    /// kinds don't mark a row. False for a row not indexed yet.
    ///
    /// While indexing, whether a row is ragged is decided against the most
    /// common field count so far, as in [`Diagnostics::report`].
    #[must_use]
    pub fn row_has_diagnostic(&self, row: usize) -> bool {
        self.read().marks.has(row)
    }

    /// The first row at or after `from` that
    /// [has a diagnostic](Diagnostics::row_has_diagnostic), for **Next**.
    /// It reads one byte per row, 64 at a time.
    #[must_use]
    pub fn next_row_with_diagnostic(&self, from: usize) -> Option<usize> {
        #[cfg(test)]
        MARK_SEARCHES.with(|n| n.set(n.get() + 1));
        self.read().marks.next(from)
    }

    /// The last row before `to` that
    /// [has a diagnostic](Diagnostics::row_has_diagnostic), for
    /// **Previous**.
    #[must_use]
    pub fn previous_row_with_diagnostic(&self, to: usize) -> Option<usize> {
        #[cfg(test)]
        MARK_SEARCHES.with(|n| n.set(n.get() + 1));
        self.read().marks.previous(to)
    }

    /// True if row `row` is ragged: not blank, and with a field count other
    /// than the most common one (so far, while indexing). The grid hatches
    /// a short ragged row's missing cells (ADR-0002 question 5).
    #[must_use]
    pub fn row_is_ragged(&self, row: usize) -> bool {
        self.read().marks.is(row, Mark::Ragged)
    }

    /// Each of rows `rows`' marks, for the gutter and the hatched cells of
    /// one screenful, under one lock. Rows not indexed yet are unmarked.
    #[must_use]
    pub fn row_flags(&self, rows: Range<usize>) -> Vec<RowFlags> {
        let shared = self.read();
        rows.map(|row| RowFlags {
            marked: shared.marks.is(row, Mark::Any),
            ragged: shared.marks.is(row, Mark::Ragged),
        })
        .collect()
    }

    /// Up to `max` rows marked as `which` says, under one lock: from `at`
    /// on in file order if `forward`, else before `at`, nearest first.
    pub(crate) fn rows_where(
        &self,
        at: usize,
        which: Mark,
        forward: bool,
        max: usize,
    ) -> Vec<usize> {
        let shared = self.read();
        let mut rows = Vec::new();
        let mut at = at;
        while rows.len() < max {
            let found = if forward {
                shared.marks.next_where(at, which)
            } else {
                shared.marks.previous_where(at, which)
            };
            let Some(row) = found else { break };
            rows.push(row);
            at = if forward { row + 1 } else { row };
        }
        rows
    }

    /// Row `row`'s code: its field count and whether it is flagged.
    pub(crate) fn code_of(&self, row: usize) -> Option<RowCode> {
        self.read().marks.code_of(row)
    }

    /// Hands each of rows `rows`' codes to `each`, in order, under one lock.
    pub(crate) fn for_each_code(&self, rows: Range<usize>, each: &mut dyn FnMut(usize, RowCode)) {
        self.read().marks.for_each_code(rows, each);
    }

    /// Up to `max` rows `pick` picks by their codes, under one lock: from
    /// `at` on in file order if `forward`, else before `at`, nearest first.
    pub(crate) fn rows_picked(
        &self,
        at: usize,
        forward: bool,
        max: usize,
        pick: &dyn Fn(RowCode) -> bool,
    ) -> Vec<(usize, RowCode)> {
        self.read().marks.rows_picked(at, forward, max, pick)
    }

    /// Whether row `row` is marked as `which` says.
    pub(crate) fn row_is(&self, row: usize, which: Mark) -> bool {
        self.read().marks.is(row, which)
    }

    /// How many rows have marks (the rows indexed so far, as of the last
    /// chunk), and the most common field count they are judged against,
    /// under one lock: for an edited row's marks (task 2.1), which are
    /// worked out from its edits against the same count.
    pub(crate) fn marked_rows_and_mode(&self) -> (usize, Option<usize>) {
        let shared = self.read();
        (shared.marks.len(), shared.marks.mode())
    }

    /// The encoding the diagnostics are for.
    #[must_use]
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    pub(crate) fn dialect(&self) -> IndexDialect {
        self.dialect
    }

    /// Publishes a chunk's results: the new report, and the marks of the
    /// rows finished since the last publish (`codes` and `wide` are
    /// emptied), with the mode they are judged against.
    pub(crate) fn publish(
        &self,
        report: Report,
        codes: &mut Vec<u8>,
        wide: &mut Vec<(u32, u32)>,
        mode: Option<usize>,
    ) {
        let done = report.is_complete();
        let report = Arc::new(report);
        // Any copy the marks need to grow (or shrink, once done) is made
        // under the read lock, so the write lock is held only for the swap
        // (`growth`, phase 1 review app-10). The old lists are dropped
        // after it is released.
        let room = self.read().marks.room(codes.len(), wide.len(), done);
        let _old = {
            let mut shared = self.shared.write().unwrap_or_else(PoisonError::into_inner);
            shared.report = report;
            shared.marks.extend(room, codes, wide, mode, done)
        };
    }

    /// How many bytes the row marks hold room for, for tests.
    #[cfg(test)]
    pub(crate) fn marks_capacity(&self) -> usize {
        self.read().marks.capacity_bytes()
    }

    /// The last report, moved out rather than copied. It takes `self`, so
    /// the lock's reference to the report is the only one left.
    pub(crate) fn into_report(self) -> Report {
        let shared = self
            .shared
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        Arc::unwrap_or_clone(shared.report)
    }

    fn read(&self) -> RwLockReadGuard<'_, Shared> {
        // A poisoned lock means a thread panicked while holding it. Every
        // write leaves the state whole before anything that could panic.
        self.shared.read().unwrap_or_else(PoisonError::into_inner)
    }
}
