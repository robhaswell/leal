//! Saving, for Swift (task 2.2, ADR-0012). See `leal_core::save` and
//! `leal_core::document::Document::save`.
//!
//! [`Document::save`] starts a [`SaveJob`] on a thread of Rust's own and
//! returns at once; the main thread never waits for it, and edits made
//! while it runs carry on. Swift awaits [`SaveJob::wait`] inside
//! `withTaskCancellationHandler`, whose handler calls [`SaveJob::cancel`]
//! (ADR-0005 decision 6): UniFFI doesn't pass Swift's cancellation through.
//! [`SaveJob::progress`] says what it is doing and how far it has got.
//!
//! - **Save** writes over the user's file ([`SaveOptions::destination`] is
//!   where it is now, `OriginalStatus.path`), after checking that it is
//!   still the one Leal opened or last saved, writable and not locked. If
//!   it changed, the save is refused with [`SaveFailure::ChangedElsewhere`];
//!   the app asks, and saves again with [`SaveOptions::overwrite_changed`].
//!   The app asks first, too, when `OriginalStatus.diverged` is set
//!   (task 2.5). [`SaveFailure::NotWritable`] and [`SaveFailure::Locked`]
//!   let it offer Duplicate or Unlock.
//! - **Save As** writes to the chosen place. From an incomplete document
//!   (a drive disconnected, the file changed while it was read, or deleted
//!   on its share) it writes the complete rows Leal trusts:
//!   [`SaveOutcome::complete`] is false, and the dialog says "about
//!   `row_count` of `estimated_row_count` rows" (ADR-0008 decision 6).
//! - **Save As UTF-8** (task 2.3, ADR-0008 decision 7) writes to the
//!   chosen place in UTF-8, from a file in any encoding: the UTF-16
//!   banner's button (mockup 06a), and the way out when Save refuses a
//!   value the file's encoding can't hold ([`SaveFailure::Unencodable`]).
//!   It refuses with [`SaveFailure::Unconvertible`], naming the cells,
//!   when some of the file's bytes aren't text in its encoding.
//! - Either way the document then *is* the saved file (ADR-0008 decision
//!   1): a new snapshot, read the same way, with a new generation, in the
//!   same lineage, so the undo history carries on by value (ADR-0012
//!   decision 4). The watcher expects the save's own change. Edits made
//!   during the save are unsaved edits afterwards
//!   ([`SaveOutcome::edits_during_save`]). For NSDocument's change count,
//!   the app takes `changeCountToken` for the edits as they were at the
//!   snapshot ([`SaveProgress::snapshot_version`], against
//!   [`Document::edit_version`] after each edit), not counts of cells. Edit
//!   versions only ever increase, across saves and re-reads too: the
//!   reading a save makes carries on from the old one's version, with the
//!   edits carried over counted within it (no version of their own), so the
//!   token noted at the version current when the save ended still matches.
//!   When the save ends, the document's cached first screen
//!   ([`Document::first_screen`]) becomes the new reading's, and the new
//!   reading's jobs are watched for panics, whether or not anyone waits.
//!
//! The core does the safe save itself (a new file on the destination's
//! volume, swapped with the old one, keeping its permissions, extended
//! attributes and Finder metadata by an explicit policy). The app passes
//! the folder `FileManager.url(for: .itemReplacementDirectory, …,
//! appropriateFor:)` makes in [`SaveOptions::folder`] (task 2.5 always
//! does); without one, or with one on another volume, the core uses a
//! hidden folder next to the file, which a sandboxed app may not be able
//! to make.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::SystemTime;

use leal_core::save::{self, SaveError};
use leal_core::source::ReadErrorKind;

use super::{Document, Finished, FirstScreen, Job, OriginalStatus, TextEncoding, to_u64, to_usize};
use crate::{CellPlace, VolumeInfo};

#[cfg(test)]
mod tests;

/// Save, Save As or Save As UTF-8. See `leal_core::save::SaveKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SaveKind {
    /// Over the user's file.
    Save,
    /// To a new place, which the document then is.
    SaveAs,
    /// To a new place, in UTF-8 whatever the file's encoding (ADR-0008
    /// decision 7): the UTF-16 banner's **Save As UTF-8…** (mockup 06a),
    /// and the way out of a value the file's encoding can't hold
    /// ([`SaveFailure::Unencodable`]). The document then is the UTF-8
    /// file: `FirstScreen.interpretation` says UTF-8, so a UTF-16
    /// document's read-only state ends.
    SaveAsUtf8,
}

/// What to save, and where.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SaveOptions {
    /// Where to write: the user's file for Save, the chosen place for Save
    /// As.
    pub destination: String,
    /// Save or Save As.
    pub kind: SaveKind,
    /// An empty folder on the destination's volume, from
    /// `FileManager.url(for: .itemReplacementDirectory, …, appropriateFor:)`,
    /// for the new file before it goes into place. The core takes it over
    /// and deletes it. `nil`, or one on another volume: a hidden folder next
    /// to the destination.
    #[uniffi(default = None)]
    pub folder: Option<String>,
    /// What the app knows about the destination's volume, as for opening:
    /// its `folder` is a second such folder, for the snapshot of the saved
    /// file.
    pub volume: VolumeInfo,
    /// The user agreed to write over a file that changed elsewhere.
    #[uniffi(default = false)]
    pub overwrite_changed: bool,
    /// How many rows the saved file's first screen has.
    pub first_screen_rows: u32,
    /// The most characters of each of its cells.
    pub max_chars: u32,
}

/// What a save is doing. See `leal_core::save::SavePhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SavePhase {
    /// Waiting for another save of the document.
    Queued,
    /// Waiting for the file to be indexed (or copied): `written` and
    /// `total` are the index pass's bytes.
    Indexing,
    /// The edits' snapshot taken (`snapshot_version`): checking the file,
    /// then writing the new file.
    Writing,
    /// Reading every row before writing on: for how new fields are quoted,
    /// or for the cells a column insert's value can't be encoded in.
    /// `written` and `total` are the bytes read and the file's length.
    /// Then writing again.
    Checking,
    /// Giving it the old file's metadata, and flushing it.
    Flushing,
    /// Putting it in place, and reading it back.
    Replacing,
    /// Done, however it ended.
    Finished,
}

/// How far a save has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SaveProgress {
    /// What it is doing.
    pub phase: SavePhase,
    /// Bytes written so far.
    pub written: u64,
    /// Bytes to write in all (the old file's length until the new one's is
    /// known); 0 before.
    pub total: u64,
    /// The document's [`edit_version`](Document::edit_version) when the
    /// save took its snapshot of the edits, once it has (the phase is
    /// `Writing` from then on): the edits up to it are in the file, later
    /// ones aren't.
    pub snapshot_version: Option<u64>,
}

/// A finished save. See `leal_core::save::Saved`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SaveOutcome {
    /// Where the file was written (a symbolic link followed).
    pub path: String,
    /// Its length in bytes.
    pub byte_count: u64,
    /// Its modification date: NSDocument's `fileModificationDate`.
    pub modified: Option<SystemTime>,
    /// How many rows it has.
    pub row_count: u64,
    /// Whether it has every row. `false` for Save As from an incomplete
    /// document.
    pub complete: bool,
    /// The document's row count, or its estimate, for "about N of M rows".
    pub estimated_row_count: u64,
    /// Edited cells that weren't saved, because their rows weren't.
    pub skipped_edits: Vec<CellPlace>,
    /// Cells edited while the save ran: unsaved edits now.
    pub edits_during_save: Vec<CellPlace>,
    /// The encoding recorded in `com.apple.TextEncoding`, if any.
    pub recorded_encoding: Option<TextEncoding>,
    /// Whether Leal's interpretation attribute records the delimiter and
    /// header choice.
    pub remembers_interpretation: bool,
    /// Metadata of the old file that couldn't be kept, for the log.
    pub skipped_metadata: Vec<String>,
    /// Whether the new file was swapped with the old one (and the old one
    /// checked), rather than renamed over it on a volume that can't swap.
    pub swapped: bool,
    /// The old file, kept rather than deleted, if the swap took out one
    /// that couldn't be checked, or that wasn't the one checked and
    /// couldn't be swapped back: it may be another app's version. The app
    /// tells the user where it is.
    pub kept_old_file: Option<String>,
    /// The saved file, as the watcher sees it now.
    pub original: OriginalStatus,
    /// The document's new first screen, read from the saved file; `nil` if
    /// it couldn't be read back (the document then reads the old snapshot,
    /// with its edits; a later save writes the same bytes).
    pub first_screen: Option<FirstScreen>,
    /// Why it couldn't be read back, for the log.
    pub reread_error: Option<String>,
}

/// Why a save didn't happen. The user's file is then as it was. See
/// `leal_core::save::SaveError`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum SaveFailure {
    /// UTF-16 files are read-only in v1: Save As UTF-8 is the way out.
    ReadOnly,
    /// These cells hold a character the file's encoding can't represent
    /// (F5: nothing is substituted), found before anything was written. The
    /// app names them and offers Save As UTF-8 (DESIGN §3.7);
    /// `Document.unencodable(value:)` gives each one's character.
    Unencodable {
        /// The file's encoding.
        encoding: TextEncoding,
        /// The cells, in order: the first 1,000.
        cells: Vec<CellPlace>,
        /// Whether there are more than those.
        more: bool,
    },
    /// Save As UTF-8: these unedited cells hold bytes that aren't text in
    /// the file's encoding (an unpaired surrogate or a final odd byte in
    /// UTF-16, a byte a single-byte encoding leaves unassigned), so they
    /// can't be converted (F5). Nothing was written; the user can edit
    /// them and try again.
    Unconvertible {
        /// The file's encoding.
        encoding: TextEncoding,
        /// The cells, in file order: the first 1,000.
        cells: Vec<CellPlace>,
        /// Whether there are more than those.
        more: bool,
    },
    /// The file would be too large for Leal to open again.
    TooLarge {
        /// Its length.
        byte_count: u64,
    },
    /// Leal doesn't have all of the file: only Save As can save it.
    Incomplete,
    /// The file's volume isn't mounted.
    Unavailable,
    /// The file on disk isn't the one Leal opened or last saved.
    ChangedElsewhere,
    /// Nothing is at the destination any more (Save).
    Missing,
    /// The file was renamed a moment ago, maybe by another app's save: try
    /// again in a moment.
    Moving,
    /// Leal may not write the file: the app offers Duplicate.
    NotWritable,
    /// The file is locked: the app offers to unlock it.
    Locked,
    /// Something other than a file (a folder, say) is at the destination.
    NotAFile,
    /// The file's drive or share was disconnected before its bytes were
    /// read.
    DriveDisconnected,
    /// The file changed while it was read.
    ChangedOnDisk,
    /// The file was deleted on its share before its bytes were read.
    DeletedElsewhere,
    /// Writing or putting the new file in place failed: a full disk, no
    /// permission.
    Io {
        /// What failed, in English, for logs.
        step: String,
        /// The OS error code, if any.
        code: Option<i32>,
        /// English, for logs.
        message: String,
    },
    /// The save was cancelled.
    Cancelled,
    /// The document failed after a panic (DESIGN §3.9).
    DocumentFailed {
        /// English, for logs.
        message: String,
    },
    /// Anything else. English, for logs.
    Internal {
        /// What went wrong.
        message: String,
    },
}

impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SaveFailure {}

fn cell_places(cells: &[(usize, usize)]) -> Vec<CellPlace> {
    cells
        .iter()
        .map(|&(row, column)| CellPlace {
            row: to_u64(row),
            column: u32::try_from(column).unwrap_or(u32::MAX),
        })
        .collect()
}

impl From<&SaveError> for SaveFailure {
    fn from(error: &SaveError) -> Self {
        match error {
            SaveError::ReadOnly => Self::ReadOnly,
            SaveError::Unencodable {
                encoding,
                cells,
                more,
            } => Self::Unencodable {
                encoding: (*encoding).into(),
                cells: cell_places(cells),
                more: *more,
            },
            SaveError::Unconvertible {
                encoding,
                cells,
                more,
            } => Self::Unconvertible {
                encoding: (*encoding).into(),
                cells: cell_places(cells),
                more: *more,
            },
            SaveError::TooLarge { len } => Self::TooLarge { byte_count: *len },
            SaveError::Incomplete => Self::Incomplete,
            SaveError::Unavailable => Self::Unavailable,
            SaveError::ChangedElsewhere => Self::ChangedElsewhere,
            SaveError::Missing => Self::Missing,
            SaveError::Moving => Self::Moving,
            SaveError::NotWritable => Self::NotWritable,
            SaveError::Locked => Self::Locked,
            SaveError::NotAFile => Self::NotAFile,
            SaveError::Read(error) => match error.kind() {
                ReadErrorKind::Disconnected => Self::DriveDisconnected,
                ReadErrorKind::ChangedOnDisk => Self::ChangedOnDisk,
                ReadErrorKind::Deleted => Self::DeletedElsewhere,
                ReadErrorKind::Cancelled => Self::Cancelled,
                ReadErrorKind::NotCopied | ReadErrorKind::Other => Self::Io {
                    step: "reading the document".to_owned(),
                    code: error.raw_os_error(),
                    message: error.to_string(),
                },
            },
            SaveError::Write { step, error } => Self::Io {
                step: (*step).to_owned(),
                code: error.raw_os_error(),
                message: error.to_string(),
            },
            SaveError::Cancelled => Self::Cancelled,
            SaveError::Failed(message) => Self::Internal {
                message: message.clone(),
            },
        }
    }
}

/// A save in progress (ADR-0005 decision 6). Releasing it doesn't cancel
/// the save: a save the user asked for finishes unless they cancel it.
#[derive(Debug, uniffi::Object)]
pub struct SaveJob {
    job: leal_core::document::SaveJob,
    document: Arc<Document>,
}

#[uniffi::export]
impl Document {
    /// Starts saving the document (see the module docs). Returns at once.
    ///
    /// # Errors
    ///
    /// [`crate::LealError::DocumentFailed`].
    pub fn save(self: Arc<Self>, options: SaveOptions) -> Result<Arc<SaveJob>, crate::LealError> {
        self.call(|| {
            let request = save::SaveRequest {
                destination: PathBuf::from(&options.destination),
                kind: match options.kind {
                    SaveKind::Save => save::SaveKind::Save,
                    SaveKind::SaveAs => save::SaveKind::SaveAs,
                    SaveKind::SaveAsUtf8 => save::SaveKind::SaveAsUtf8,
                },
                folder: options.folder.map(PathBuf::from),
                volume: options.volume.into(),
                overwrite_changed: options.overwrite_changed,
                first_screen_rows: to_usize(options.first_screen_rows),
                max_chars: to_usize(options.max_chars),
            };
            let job = self.document.save(request);
            // A panic in the save fails the document (DESIGN §3.9), as one
            // in any of its jobs.
            self.failure.watch(job.job().control());
            // The bookkeeping after a rebase happens as the job ends,
            // whether anyone waits for it or not: the cached first screen
            // (if it is newer), and watching the new reading's jobs.
            let document = Arc::clone(&self);
            let finished = job.clone();
            job.job().control().on_finish(move || {
                if let Some(Ok(saved)) = finished.result()
                    && let Some(screen) = &saved.reread
                {
                    {
                        let mut cached = document
                            .first_screen
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        if screen.generation > cached.generation {
                            *cached = FirstScreen::from(screen.clone());
                        }
                    }
                    document.watch_jobs();
                }
                // A drive back during the save, reconnected as it ended:
                // after the save's own screen, which a restart follows.
                if let Some(restarted) = finished.restarted() {
                    document.adopt_restart(restarted);
                }
            });
            Ok(Arc::new(SaveJob {
                job,
                document: Arc::clone(&self),
            }))
        })
    }
}

#[uniffi::export]
impl SaveJob {
    /// Stops the save at its next checkpoint: while it waits for another
    /// save or for the index pass, or within one chunk of writing. Before
    /// the new file is in place, nothing is saved and the user's file is as
    /// it was; after, the save finishes.
    pub fn cancel(&self) {
        self.job.cancel();
    }

    /// What it is doing, and how far it has got.
    #[must_use]
    pub fn progress(&self) -> SaveProgress {
        let progress = self.job.progress();
        SaveProgress {
            phase: match progress.phase {
                save::SavePhase::Queued => SavePhase::Queued,
                save::SavePhase::Indexing => SavePhase::Indexing,
                save::SavePhase::Writing => SavePhase::Writing,
                save::SavePhase::Checking => SavePhase::Checking,
                save::SavePhase::Flushing => SavePhase::Flushing,
                save::SavePhase::Replacing => SavePhase::Replacing,
                save::SavePhase::Finished => SavePhase::Finished,
            },
            written: progress.written,
            total: progress.total,
            snapshot_version: progress.snapshot_version,
        }
    }

    /// The generation the document was read again at as the save ended,
    /// if it was: a check of the file during the save found its removable
    /// drive back, and the reconnecting waited for the save. Set before the
    /// save finishes, whatever its outcome; the app then does what it does
    /// when `checkOriginal` reads the file again (new jobs, diagnostics and
    /// review, the same first screen).
    #[must_use]
    pub fn restarted(&self) -> Option<u64> {
        self.job.restarted().map(|restarted| restarted.to)
    }

    /// The save's job, for its id.
    #[must_use]
    pub fn job(&self) -> Arc<Job> {
        Arc::new(Job {
            control: self.job.job().control().clone(),
        })
    }

    /// Waits for the save, without blocking a thread, and gives its
    /// outcome. (UniFFI makes it `async throws` in Swift; the error is
    /// always a [`SaveFailure`].)
    ///
    /// # Errors
    ///
    /// The [`SaveFailure`].
    pub async fn wait(&self) -> Result<SaveOutcome, SaveFailure> {
        let _ = Finished::new(self.job.job().control().clone()).await;
        self.outcome()
    }
}

impl SaveJob {
    /// The finished save's outcome.
    fn outcome(&self) -> Result<SaveOutcome, SaveFailure> {
        if let Some(message) = self.document.failure.get() {
            return Err(SaveFailure::DocumentFailed { message });
        }
        let saved = match self.job.result() {
            Some(Ok(saved)) => saved,
            Some(Err(error)) => return Err(error.into()),
            None => {
                return Err(SaveFailure::Internal {
                    message: "the save hasn't finished".to_owned(),
                });
            }
        };
        // As the job's own callback does, in case this runs first.
        if let Some(screen) = &saved.reread {
            let mut cached = self
                .document
                .first_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if screen.generation > cached.generation {
                *cached = FirstScreen::from(screen.clone());
            }
        }
        Ok(SaveOutcome {
            path: saved.path.to_string_lossy().into_owned(),
            byte_count: saved.len,
            modified: saved.modified,
            row_count: to_u64(saved.rows),
            complete: saved.complete,
            estimated_row_count: to_u64(saved.estimated_rows),
            skipped_edits: cell_places(&saved.skipped_edits),
            edits_during_save: cell_places(&saved.edits_during_save),
            recorded_encoding: saved.attributes.text_encoding.map(TextEncoding::from),
            remembers_interpretation: saved.attributes.interpretation.is_some(),
            skipped_metadata: saved.skipped_metadata.clone(),
            swapped: saved.placed == save::Placed::Swapped,
            kept_old_file: saved
                .kept
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            original: saved.original.clone().into(),
            first_screen: saved.reread.clone().map(FirstScreen::from),
            reread_error: saved.reread_error.clone(),
        })
    }
}
