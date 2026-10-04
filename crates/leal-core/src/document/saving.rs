//! Saving a document (task 2.2, DESIGN §3.7, ADR-0012): the splice writer,
//! as a job, and the rebase onto the file it wrote (ADR-0008 decision 1).
//!
//! [`Document::save`] starts a [`SaveJob`] on a thread of its own
//! (`leal-save`, P1: a save the user asked for never pauses for them). The
//! main thread never waits for it (DESIGN §3.9): the save takes the
//! document's writer lock only for a moment at the start, and at the end
//! to carry edits over and make the new reading current; no file is
//! touched under it. It:
//!
//! 1. waits its turn behind any other save of the document (not in the
//!    order they were asked for: whichever looks first), then for the
//!    index pass to finish, so every row (and for a file on a removable
//!    drive or a share, its copy) is there; both waits can be cancelled;
//! 2. takes a snapshot of the edits (an `Arc` of the overlay, and its
//!    version, [`SaveProgress::snapshot_version`]) under the writer lock,
//!    and lets go: edits carry on;
//! 3. for Save, checks the user's file afresh: still the one Leal opened or
//!    last saved (ADR-0008 decision 9), writable, and not locked (ADR-0012
//!    decision 1);
//! 4. writes the new file next to the destination ([`Staged`]), walking
//!    the rows in order through the piece list (task 2.4c), making each
//!    row's splices as it reaches it (edited rows, deleted rows, inserted
//!    rows, and rows whose neighbours changed): the
//!    snapshot's bytes a chunk at a time with a checkpoint between chunks
//!    (ADR-0005 decision 6). On a volume that can vanish it also tees the
//!    bytes to a copy on the internal disk. Save As UTF-8 (task 2.3)
//!    converts the snapshot's bytes to UTF-8 as it copies them, a chunk at
//!    a time; bytes that aren't text there name their cells, and the save
//!    is refused once the file has been read ([`SaveError::Unconvertible`]);
//! 5. gives it the old file's metadata, best-effort, and the two
//!    attributes ([`AttributePlan`]), and orders it onto the disk; makes
//!    the document's next snapshot of it (the tee copy, or a clone), and
//!    closes it;
//! 6. under the watcher's lock (not the writer lock): checks the file once
//!    more (if its metadata changed meanwhile, checks again that it may be
//!    replaced, and copies the metadata again), and puts the new one in
//!    place: swapped, and the one swapped out checked, where the volume
//!    can swap; renamed over it where it can't ([`Placed`]);
//! 7. rebases: makes the new reading with no lock held, its index built
//!    from the plan (each row's start and field count in the new file,
//!    noted as it was written, task 2.4c), so every row reads at once, the
//!    column count is right at once, and rows and columns can be inserted
//!    and deleted again at once; the index pass runs again in the
//!    background only for the diagnostics and the review. A file converted
//!    to UTF-8
//!    moved every byte, so its new reading waits for that pass instead
//!    (about 60 ms per 100 MB), before it is current. Then, under the
//!    writer lock, carries the edits made during the save (after step 2)
//!    over to it by value, as unsaved edits, and makes it current.
//!
//! A cancel, or any failure, before the new file is in place deletes what
//! was written and leaves the user's file as it was. From then on the
//! save has succeeded, whatever else fails.
//!
//! While a save runs, [`Document::reinterpret`] is refused
//! ([`DocumentError::Saving`]), and a drive that comes back isn't
//! reconnected until it ends ([`SaveJob::restarted`]). A search started
//! before it keeps searching the reading it started on; the app starts it
//! again on the new generation, as after a re-read.
//!
//! [`EditStore::rebased`]: crate::edit::EditStore::rebased

use std::cell::Cell;
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError};
use std::time::Duration;

use super::view::{RowView, ViewCell};
use super::{
    Context, Document, FirstScreen, Reading, Restarted, inserted_row, read_first_paint, start_jobs,
};
use crate::detect::{CensusStream, Choices, FIRST_PAINT_BYTES};
use crate::diagnostics::RowMarks;
use crate::dialect::{Bom, Encoding};
use crate::edit::{EditStore, Overlay, RowEdits, RowId};
use crate::index::{RowIndex, Status};
use crate::save::{
    AttributeFacts, AttributePlan, Fix, Placed, SaveError, SaveKind, SavePhase, SaveProgress,
    SaveRequest, Saved, Splice, encode, needs_census,
};
use crate::schedule::{Interval, JobError, JobHandle, Priority};
use crate::source::{
    Existing, FileIdentity, INTERPRETATION_ATTRIBUTE_C, OriginalState, Put, RawAttributes, Source,
    Staged, SwapError, TEXT_ENCODING_ATTRIBUTE_C, VolumeInfo, VolumeKind, as_on_disk, can_write,
    kind_of_folder, look_afresh, same_file,
};

use super::editing::Target;

mod sink;
mod walk;

#[cfg(any(test, feature = "test-hooks"))]
use sink::Collect;
use sink::FileSink;
use walk::stream;

/// How much of the snapshot the writer copies between checkpoints: 1 MiB,
/// a fraction of a millisecond from a map (DESIGN §3.10 rule 3).
pub const SAVE_CHUNK_BYTES: usize = 1 << 20;

/// The chunk size tests use instead, so small files cross chunks.
#[cfg(test)]
pub(crate) static TEST_CHUNK_BYTES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(SAVE_CHUNK_BYTES);

/// How much of the snapshot the writer copies between checkpoints.
fn chunk_bytes() -> usize {
    #[cfg(test)]
    {
        TEST_CHUNK_BYTES.load(Ordering::Relaxed).max(1)
    }
    #[cfg(not(test))]
    {
        SAVE_CHUNK_BYTES
    }
}

/// How often a save waiting (for another save, or the index pass) looks
/// whether it was cancelled.
const POLL: Duration = Duration::from_millis(20);

/// How many edited rows the writer makes splices for between checkpoints.
const ROWS_PER_CHECKPOINT: usize = 256;

/// What a test hook is called with: each checkpoint's number, from 0, and
/// [`AT_SWAP`] just before the new file goes into place.
#[cfg(test)]
pub(crate) type ChunkHook = Arc<dyn Fn(usize) + Send + Sync>;

/// What a test hook is called with just before the new file goes into
/// place, under the watcher's lock.
#[cfg(test)]
pub(crate) const AT_SWAP: usize = usize::MAX;

/// What a test hook is called with once the new reading is made, before
/// it takes the writer lock to become current.
#[cfg(test)]
pub(crate) const BEFORE_ADOPT: usize = usize::MAX - 1;

/// No such points outside tests.
#[cfg(not(test))]
const AT_SWAP: usize = 0;
#[cfg(not(test))]
const BEFORE_ADOPT: usize = 0;

/// No hook outside tests.
#[cfg(not(test))]
type ChunkHook = std::convert::Infallible;

/// How many rows the check for "the file quotes every field" reads at once.
const QUOTE_SCAN_ROWS: usize = 4096;

/// A save in progress (ADR-0005 decision 6): cancel it, see how far it has
/// got, or wait for it.
#[derive(Debug, Clone)]
pub struct SaveJob {
    job: JobHandle<Saved>,
    shared: Arc<SaveShared>,
}

/// What the save's job and its handle share.
#[derive(Debug, Default)]
struct SaveShared {
    phase: AtomicU8,
    written: AtomicU64,
    total: AtomicU64,
    /// The edits' version at the snapshot, once it is taken.
    snapshot_version: OnceLock<u64>,
    /// The document, read again after the save ended (a drive back).
    restarted: OnceLock<Restarted>,
    /// Why the save failed, once it has.
    error: OnceLock<SaveError>,
}

impl SaveShared {
    fn set_phase(&self, phase: SavePhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    fn phase(&self) -> SavePhase {
        const PHASES: [SavePhase; 6] = [
            SavePhase::Queued,
            SavePhase::Indexing,
            SavePhase::Writing,
            SavePhase::Flushing,
            SavePhase::Replacing,
            SavePhase::Finished,
        ];
        PHASES
            .get(usize::from(self.phase.load(Ordering::Relaxed)))
            .copied()
            .unwrap_or_default()
    }
}

impl SaveJob {
    /// Stops the save at its next checkpoint: while it waits for another
    /// save or for the index pass, or within one chunk of writing. Before
    /// the new file is in place the save is then abandoned: what it wrote
    /// is deleted and the user's file is as it was. Once it is in place,
    /// the save finishes.
    pub fn cancel(&self) {
        self.job.cancel();
    }

    /// The job, for its id, to wait on, or to watch for a panic.
    #[must_use]
    pub fn job(&self) -> &JobHandle<Saved> {
        &self.job
    }

    /// What it is doing, and how many bytes it has written of how many.
    #[must_use]
    pub fn progress(&self) -> SaveProgress {
        SaveProgress {
            phase: self.shared.phase(),
            written: self.shared.written.load(Ordering::Relaxed),
            total: self.shared.total.load(Ordering::Relaxed),
            snapshot_version: self.shared.snapshot_version.get().copied(),
        }
    }

    /// Whether, once the save ended, the document was read again: a check
    /// of the user's file during the save found its removable drive back
    /// ([`Document::check_original_restarting`]), and left reconnecting
    /// until the save ended. Set before the job finishes.
    #[must_use]
    pub fn restarted(&self) -> Option<Restarted> {
        self.shared.restarted.get().copied()
    }

    /// Waits for the save and gives its result. Don't call it on the main
    /// thread.
    ///
    /// # Errors
    ///
    /// Why the save didn't happen.
    pub fn wait(&self) -> Result<&Saved, &SaveError> {
        let _ = self.job.control().wait();
        self.result().unwrap_or_else(unreachable_running)
    }

    /// The save's result, or `None` while it runs.
    #[must_use]
    pub fn result(&self) -> Option<Result<&Saved, &SaveError>> {
        Some(match self.job.result()? {
            Ok(saved) => Ok(saved),
            Err(JobError::Cancelled) => Err(self.shared.error.get_or_init(|| SaveError::Cancelled)),
            Err(JobError::Panicked(message)) => Err(self
                .shared
                .error
                .get_or_init(|| SaveError::Failed(format!("the save panicked: {message}")))),
            Err(other) => Err(self
                .shared
                .error
                .get_or_init(|| SaveError::Failed(other.to_string()))),
        })
    }
}

/// `wait` returns only once the job has finished, so it has a result.
fn unreachable_running<'a>() -> Result<&'a Saved, &'a SaveError> {
    static STILL_RUNNING: OnceLock<SaveError> = OnceLock::new();
    Err(STILL_RUNNING.get_or_init(|| SaveError::Failed("the save job hadn't finished".to_owned())))
}

/// The document's turn to save, held by one save at a time; giving it up
/// lets the file be read again ([`Document::reinterpret`]).
struct Turn<'a>(&'a AtomicBool);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What part of the reading a save writes, and in what encoding.
struct Extent<'r> {
    /// Every row, and the whole file.
    complete: bool,
    /// The index the rows are in (the reading's, or the first 64 KB's).
    index: &'r RowIndex,
    rows: usize,
    /// The snapshot is written up to here.
    end: usize,
    /// The document's encoding.
    source: Encoding,
    /// The encoding written ([`SaveKind::writes`]).
    target: Encoding,
}

impl Extent<'_> {
    /// Whether the file's bytes are converted as they are written (Save As
    /// UTF-8 from another encoding).
    fn converts(&self) -> bool {
        self.source != self.target
    }
}

/// What writing the new file found, for the rebase.
#[derive(Debug, Default)]
struct Streamed {
    fixes: Vec<Fix>,
    /// The logical rows written.
    rows: usize,
    /// Each row's start in the new file, for its index (`None` when
    /// converting, or if the file would be too large).
    starts: Option<Vec<u32>>,
    /// Each row's field count in the new file, for its column operations
    /// (`None` when converting, or if the old file's weren't all known).
    counts: Option<RowMarks>,
    /// The new file's most common field count: from `counts`, or else the
    /// old one's as the column operations made it.
    mode: Option<usize>,
    len: u64,
    /// The old file's unterminated quote: where it is in the new one, or
    /// gone if its cell was edited (the quote closed).
    unterminated: Option<usize>,
}

/// Where the writer's output goes: a new file, or (tests) a list of the
/// splices.
trait Sink {
    /// The snapshot's bytes in `range`, unchanged (or, for Save As UTF-8,
    /// converted).
    fn copy(&mut self, range: Range<usize>) -> Result<(), SaveError>;
    /// A splice's bytes.
    fn splice(&mut self, splice: Splice) -> Result<(), SaveError>;
    /// Save As UTF-8: these cells can't be converted, so the save will be
    /// refused; their row isn't written. An error stops the save at once
    /// (once [`MAX_NAMED_CELLS`] are named).
    fn refuse(&mut self, cells: &[(usize, usize)]) -> Result<(), SaveError>;
}

impl Document {
    /// Starts saving the document (see the module docs). Returns at once;
    /// the [`SaveJob`] says how it went. The document is shared with the
    /// job, which holds it until it ends.
    #[must_use]
    pub fn save(self: &Arc<Self>, request: SaveRequest) -> SaveJob {
        self.start_save(request, None)
    }

    /// [`save`](Self::save), calling `hook` with each checkpoint's number
    /// first (tests: they hold the save part-way).
    #[cfg(test)]
    pub(crate) fn save_hooked(self: &Arc<Self>, request: SaveRequest, hook: ChunkHook) -> SaveJob {
        self.start_save(request, Some(hook))
    }

    fn start_save(self: &Arc<Self>, request: SaveRequest, hook: Option<ChunkHook>) -> SaveJob {
        let shared = Arc::new(SaveShared::default());
        let document = Arc::clone(self);
        let progress = Arc::clone(&shared);
        let job = self
            .scheduler
            .spawn(Priority::P1, Interval::Save, move |job| {
                let checkpoints = Cell::new(0);
                let checkpoint = || {
                    #[cfg(test)]
                    if let Some(hook) = &hook {
                        hook(checkpoints.get());
                    }
                    checkpoints.set(checkpoints.get() + 1);
                    job.checkpoint().map_err(|_| SaveError::Cancelled)
                };
                let reached = |point: usize| {
                    #[cfg(test)]
                    if let Some(hook) = &hook {
                        hook(point);
                    }
                    #[cfg(not(test))]
                    let _ = (&hook, point);
                };
                let result = document.save_now(&request, &checkpoint, &reached, &progress);
                // Its turn given up: a check that found the drive back
                // meanwhile reconnects now. (`SeqCst` with the check's own
                // order, `recheck` then `saving`: one of the two sees the
                // other, so a check is never left undone.)
                if document.recheck.swap(false, Ordering::SeqCst)
                    && let (_, Some(restarted)) = document.check_original_restarting()
                {
                    let _ = progress.restarted.set(restarted);
                }
                progress.set_phase(SavePhase::Finished);
                match result {
                    Ok(saved) => Ok(saved),
                    Err(SaveError::Cancelled) => Err(JobError::Cancelled),
                    Err(error) => {
                        let message = error.to_string();
                        let _ = progress.error.set(error);
                        Err(JobError::Failed(message))
                    }
                }
            });
        SaveJob { job, shared }
    }

    /// TEST HOOK, not for product code: what a save would write now (the
    /// splices over the snapshot, the rows, the fixes) without writing
    /// anything. It waits for the index pass. For Save As UTF-8 from
    /// another encoding, the splices are in UTF-8, the bytes between them
    /// aren't converted (so nothing they hold is refused), and the length
    /// is the snapshot's with the splices.
    ///
    /// # Errors
    ///
    /// As for a save, before it writes.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn save_plan(&self, kind: SaveKind) -> Result<crate::save::SavePlan, SaveError> {
        let reading = self.indexed_reading(&|| Ok(()), None)?;
        let overlay = reading.edits.overlay();
        let extent = extent_of(&reading, kind)?;
        check_encodable(&reading, &overlay, &extent)?;
        let mut sink = Collect::default();
        let streamed = stream(&reading, &overlay, &extent, &mut sink, &|| Ok(()))?;
        if !sink.refused.is_empty() {
            return Err(SaveError::Unconvertible {
                encoding: extent.source,
                cells: sink.refused,
                more: false,
            });
        }
        Ok(crate::save::SavePlan {
            splices: sink.splices,
            end: extent.end,
            rows: streamed.rows,
            complete: extent.complete,
            skipped: skipped_edits(&reading, &overlay, streamed.rows),
            fixes: streamed.fixes,
            len: streamed.len,
        })
    }

    /// TEST HOOK, not for product code: a save's work without the disk:
    /// the plan, the bytes written to `out`, and the attributes' census,
    /// for a benchmark that catches a slower writer (the disk's time is in
    /// `save/one_edit`). Returns the bytes written.
    ///
    /// # Errors
    ///
    /// As for a save, before it puts the file in place.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn save_to_writer(&self, kind: SaveKind, out: &mut dyn Write) -> Result<u64, SaveError> {
        let reading = self.indexed_reading(&|| Ok(()), None)?;
        let overlay = reading.edits.overlay();
        let extent = extent_of(&reading, kind)?;
        check_encodable(&reading, &overlay, &extent)?;
        let progress = SaveShared::default();
        let mut census = (!extent.converts()).then(CensusStream::default);
        let mut sink = FileSink::new(
            &reading,
            overlay.map(),
            &extent,
            out,
            &progress,
            census.as_mut(),
            &|| Ok(()),
        );
        let mut streamed = stream(&reading, &overlay, &extent, &mut sink, &|| Ok(()))?;
        sink.finish(&mut streamed)?;
        Ok(streamed.len)
    }

    /// The current reading once its index pass has finished, waiting for it
    /// (and calling `checkpoint` while it waits).
    fn indexed_reading(
        &self,
        checkpoint: &dyn Fn() -> Result<(), SaveError>,
        progress: Option<&SaveShared>,
    ) -> Result<Arc<Reading>, SaveError> {
        let reading = self.current();
        while reading.index_job.control().wait_timeout(POLL).is_none() {
            if let Some(progress) = progress {
                progress
                    .written
                    .store(to_u64(reading.index.bytes_scanned()), Ordering::Relaxed);
                progress
                    .total
                    .store(reading.source.len(), Ordering::Relaxed);
            }
            checkpoint()?;
        }
        Ok(reading)
    }

    /// This save's turn: the first save of the document takes it at once,
    /// a later one waits (and can be cancelled meanwhile).
    fn take_turn(
        &self,
        checkpoint: &dyn Fn() -> Result<(), SaveError>,
    ) -> Result<Turn<'_>, SaveError> {
        loop {
            if self
                .saving
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return Ok(Turn(&self.saving));
            }
            checkpoint()?;
            std::thread::sleep(POLL);
        }
    }

    /// The save itself, on the job's thread.
    fn save_now(
        &self,
        request: &SaveRequest,
        checkpoint: &dyn Fn() -> Result<(), SaveError>,
        reached: &dyn Fn(usize),
        progress: &SaveShared,
    ) -> Result<Saved, SaveError> {
        // 1 and 2: this save's turn, the whole file indexed, then the edits'
        // snapshot, under the writer lock for a moment.
        progress.set_phase(SavePhase::Queued);
        let _turn = self.take_turn(checkpoint)?;
        progress.set_phase(SavePhase::Indexing);
        let (reading, overlay, version) = loop {
            let reading = self.indexed_reading(checkpoint, Some(progress))?;
            let _one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
            let now = self.current();
            if Arc::ptr_eq(&now, &reading) {
                let (overlay, version) = now.edits.snapshot();
                // Under the lock: an edit made after the app sees these is
                // after the snapshot.
                let _ = progress.snapshot_version.set(to_u64(version));
                progress.written.store(0, Ordering::Relaxed);
                progress.set_phase(SavePhase::Writing);
                break (now, overlay, version);
            }
        };
        let kind = request.kind;
        let destination = resolve_link(&request.destination);

        // 3: the checks before writing.
        let existing = match kind {
            SaveKind::Save => {
                if !reading.source.can_save() {
                    return Err(SaveError::Incomplete);
                }
                if self.original.status().state == OriginalState::Unavailable {
                    return Err(SaveError::Unavailable);
                }
                if self.original.is_moving() {
                    return Err(SaveError::Moving);
                }
                Some(self.check_before_writing(&destination, request)?)
            }
            SaveKind::SaveAs | SaveKind::SaveAsUtf8 => existing_at(&destination)?,
        };
        let extent = extent_of(&reading, kind)?;
        check_encodable(&reading, &overlay, &extent)?;

        // 4: the new file. Its length is known once it is written; until
        // then, the snapshot's bytes stand in for it.
        progress.total.store(to_u64(extent.end), Ordering::Relaxed);
        let write = |step| move |error| SaveError::Write { step, error };
        let temps = reading.source.temp_folders();
        let volume = saved_volume(&reading, &destination, request);
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let kind_of_volume =
            kind_of_folder(parent, temps, &volume).map_err(write("looking at the folder"))?;
        let mut staged = Staged::create(
            temps,
            request.folder.clone(),
            &destination,
            kind_of_volume != VolumeKind::Fixed,
        )
        .map_err(write("making the new file"))?;
        let detection = &reading.detection;
        let had_text_encoding = reading.source.attributes().text_encoding.is_some();
        // Save As UTF-8 records the encoding whatever a reopen would guess.
        let utf8 = kind == SaveKind::SaveAsUtf8;
        let mut census =
            (needs_census(detection, had_text_encoding) && !utf8).then(CensusStream::default);
        let (mut streamed, head) = {
            let mut out = BufWriter::with_capacity(SAVE_CHUNK_BYTES, staged.writer());
            let mut sink = FileSink::new(
                &reading,
                overlay.map(),
                &extent,
                &mut out,
                progress,
                census.as_mut(),
                checkpoint,
            );
            let mut streamed = stream(&reading, &overlay, &extent, &mut sink, checkpoint)?;
            let head = sink.finish(&mut streamed)?;
            out.flush().map_err(write("writing the new file"))?;
            (streamed, head)
        };
        // Written in full. (Converting, the progress counted the bytes read.)
        progress.total.store(streamed.len, Ordering::Relaxed);
        progress.written.store(streamed.len, Ordering::Relaxed);

        // 5: metadata, attributes, flushed, the next snapshot, closed.
        progress.set_phase(SavePhase::Flushing);
        let mut skipped_metadata = match &existing {
            Some(existing) => staged
                .copy_attributes(existing)
                .map_err(write("copying the file's attributes"))?,
            None => Vec::new(),
        };
        let attributes = AttributePlan::decide(&AttributeFacts {
            detection,
            had_text_encoding,
            head: &head,
            len: streamed.len,
            census: census.map(CensusStream::finish),
            utf8,
        });
        let text_encoding = attributes.text_encoding_value();
        let interpretation = attributes.interpretation_value();
        let ours = [
            (
                TEXT_ENCODING_ATTRIBUTE_C,
                text_encoding.as_deref().map(str::as_bytes),
            ),
            (
                INTERPRETATION_ATTRIBUTE_C,
                interpretation.as_deref().map(str::as_bytes),
            ),
        ];
        for (name, value) in ours {
            if staged.set_attribute(name, value).is_err() {
                skipped_metadata.push(name.to_string_lossy().into_owned());
            }
        }
        skipped_metadata.extend(
            staged
                .finish(existing.as_ref())
                .map_err(write("flushing the new file"))?,
        );
        // Only its status-change time is needed from here: the old file
        // isn't held open across the swap and its deletion (on a share,
        // an open file can't be deleted, only renamed aside).
        let changed_before = existing.as_ref().map(Existing::changed);
        drop(existing);
        let cancelled = Cell::new(false);
        let snapshot = staged.snapshot(temps, volume.folder.clone(), &|| {
            cancelled.set(checkpoint().is_err());
            cancelled.get()
        });
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(_) if cancelled.get() => return Err(SaveError::Cancelled),
            Err(error) => return Err(write("making the document's snapshot")(error)),
        };
        checkpoint()?;

        // 6: into place, under the watcher's lock only: edits carry on.
        progress.set_phase(SavePhase::Replacing);
        let checked: Cell<Option<FileIdentity>> = Cell::new(None);
        let changed: Cell<Option<Existing>> = Cell::new(None);
        let redone: Cell<Option<Vec<String>>> = Cell::new(None);
        let put: Cell<Option<Put>> = Cell::new(None);
        let original = self.original.replace_with(
            &destination,
            |opened, diverged| {
                reached(AT_SWAP);
                if kind != SaveKind::Save {
                    return Ok(());
                }
                if diverged && !request.overwrite_changed {
                    return Err(SaveError::ChangedElsewhere);
                }
                let now = unchanged(&destination, opened, request)?;
                checked.set(Some(*now.identity()));
                if changed_before != Some(now.changed()) {
                    // Its permissions, flags or attributes changed while
                    // the save ran (the contents didn't): checked again,
                    // and its metadata copied again.
                    may_replace(&destination, &now)?;
                    changed.set(Some(now));
                }
                Ok(())
            },
            || {
                if let Some(now) = changed.take() {
                    let skipped = staged
                        .copy_metadata_again(&now, &ours)
                        .map_err(write("copying the file's metadata again"))?;
                    redone.set(Some(skipped));
                }
                let done = staged
                    .swap_into(&destination, checked.get().as_ref())
                    .map_err(|error| match error {
                        SwapError::Changed => SaveError::ChangedElsewhere,
                        SwapError::Missing => SaveError::Missing,
                        SwapError::NotAFile => SaveError::NotAFile,
                        SwapError::Locked => SaveError::Locked,
                        SwapError::NotWritable => SaveError::NotWritable,
                        SwapError::Io(error) => SaveError::Write {
                            step: "putting the new file in place",
                            error,
                        },
                    })?;
                let identity = done.identity;
                put.set(Some(done));
                Ok(identity)
            },
        )?;
        // The new file is in place: the save has succeeded, whatever
        // follows.
        if let Some(skipped) = redone.take() {
            skipped_metadata = skipped;
        }
        let (placed, kept) = put
            .take()
            .map_or((Placed::Renamed, None), |put| (put.placed, put.kept));
        let identity = self.original.opened();
        let raw = RawAttributes {
            text_encoding: text_encoding.map(String::into_bytes),
            interpretation: interpretation.map(String::into_bytes),
        };
        let estimated_rows = super::estimated_rows(&reading, &reading.head, reading.source.len());
        let mut skipped_edits = skipped_edits(&reading, &overlay, streamed.rows);
        // The rebase: the new reading is made without the writer lock, and
        // takes it only to carry the edits over and become current.
        let adopted =
            Source::from_snapshot(&destination, snapshot, identity, raw, temps, kind_of_volume)
                .map_err(|error| error.to_string())
                .and_then(|source| {
                    let starts = streamed.starts.take();
                    let counts = streamed.counts.take();
                    let plan = (starts, counts);
                    self.rebuilt(&reading, &Arc::new(source), &extent, &streamed, plan)
                })
                .and_then(|new| {
                    reached(BEFORE_ADOPT);
                    self.adopt(&reading, new, &overlay, version, streamed.rows)
                });
        // Deletes the old file a swap left in the new file's folder (unless
        // it was kept), with no lock held.
        drop(staged);
        let (reread, reread_error, edits_during_save) = match adopted {
            Ok(Adopted {
                reading: new,
                column_count,
                carried,
                lost,
            }) => {
                skipped_edits.extend(lost);
                let rows = Document::read_rows_of(&new, 0..request.first_screen_rows, |view| {
                    super::row_cells(&view, request.max_chars)
                })
                .unwrap_or_default();
                let screen = FirstScreen {
                    generation: new.generation,
                    detection: new.detection.clone(),
                    rows,
                    row_count: streamed.rows,
                    estimated_row_count: streamed.rows,
                    column_count,
                };
                (Some(screen), None, carried)
            }
            Err(error) => {
                // Still unsaved in the old reading: the cells edited since
                // the snapshot.
                let cells = touched_cells(&reading, &overlay, version)
                    .into_iter()
                    .flat_map(|(row, columns)| columns.into_iter().map(move |column| (row, column)))
                    .collect();
                (None, Some(error), cells)
            }
        };
        Ok(Saved {
            path: as_on_disk(&destination),
            len: streamed.len,
            rows: streamed.rows,
            complete: extent.complete,
            estimated_rows,
            skipped_edits,
            edits_during_save,
            fixes: streamed.fixes,
            attributes,
            skipped_metadata,
            placed,
            kept,
            modified: identity.modified,
            original,
            reread,
            reread_error,
        })
    }

    /// The check before writing (ADR-0008 decision 9, ADR-0012 decision 1):
    /// the file at `destination`, opened afresh, must be there, a regular
    /// file, not locked, writable by Leal, and the one Leal opened or last
    /// saved, unchanged, unless the user agreed to write over it.
    fn check_before_writing(
        &self,
        destination: &Path,
        request: &SaveRequest,
    ) -> Result<Existing, SaveError> {
        if self.original.status().diverged && !request.overwrite_changed {
            return Err(SaveError::ChangedElsewhere);
        }
        if existing_at(destination)?.is_none() {
            return Err(SaveError::Missing);
        }
        let existing = unchanged(destination, &self.original.opened(), request)?;
        may_replace(destination, &existing)?;
        Ok(existing)
    }

    /// The new reading of the file a save wrote, whose snapshot is
    /// `source`, split into cells the way `old` was, in `old`'s lineage,
    /// with no edits yet. Its rows come from an index built from the save's
    /// plan (`starts`, task 2.4c), complete at once, with the field count
    /// mode of the rows written, so they read at once and rows can be
    /// inserted and deleted again; with each row's field count (`counts`),
    /// so can columns. Its index pass runs again for the diagnostics and
    /// the review. Or why it couldn't be made (the document then keeps
    /// reading `old`).
    fn rebuilt(
        &self,
        old: &Reading,
        source: &Arc<Source>,
        extent: &Extent<'_>,
        streamed: &Streamed,
        (starts, counts): (Option<Vec<u32>>, Option<RowMarks>),
    ) -> Result<Rebuilt, String> {
        let head: Arc<[u8]> = Arc::from(
            &*source
                .read_head(FIRST_PAINT_BYTES)
                .map_err(|error| error.to_string())?,
        );
        // As a reopen would read it, with the attributes just written and
        // the user's own choices. If that splits it another way (the
        // attributes didn't suffice), it is read the old way regardless:
        // the undo history's commands are tied to the split. Converted to
        // UTF-8 (Save As UTF-8), the file is UTF-8, with a UTF-8 BOM if it
        // had a BOM, and an encoding the user chose no longer applies.
        let converts = extent.converts();
        let (encoding, bom) = if converts {
            let bom = if old.detection.bom == Bom::None {
                Bom::None
            } else {
                Bom::Utf8
            };
            (extent.target, bom)
        } else {
            (old.detection.encoding, old.detection.bom)
        };
        let choices = if converts {
            Choices {
                encoding: None,
                ..old.choices
            }
        } else {
            old.choices
        };
        let mut paint =
            read_first_paint(source, &head, choices).map_err(|error| error.to_string())?;
        let same_split = |d: &crate::detect::Detection| {
            d.delimiter == old.detection.delimiter
                && d.encoding == encoding
                && d.bom == bom
                && d.header == old.detection.header
        };
        if !same_split(&paint.detection) {
            let forced = Choices {
                delimiter: Some(old.detection.delimiter),
                header: Some(old.detection.header),
                encoding: Some(encoding),
            };
            paint = read_first_paint(source, &head, forced).map_err(|error| error.to_string())?;
        }
        let column_count = if converts {
            // Every byte moved: the rows are the new index pass's, which
            // the rebase waits for (`Rebuilt::wait`).
            paint.head_index.field_count_mode().unwrap_or(0)
        } else {
            let len = usize::try_from(streamed.len).map_err(|error| error.to_string())?;
            let index = starts
                .and_then(|starts| {
                    RowIndex::from_starts(
                        extent.index.dialect(),
                        starts,
                        len,
                        streamed.mode,
                        streamed.unterminated,
                    )
                })
                .ok_or("the saved file's rows don't add up")?;
            let column_count = index.field_count_mode().unwrap_or(0);
            // The rows are served from it; the index pass fills one of its
            // own, for the diagnostics and the field count mode.
            paint.index = Arc::new(index);
            column_count
        };
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let edits = Arc::new(EditStore::rebased(old.edits.lineage()));
        let context = Context {
            source,
            head: &head,
            scheduler: &self.scheduler,
            progress: self.progress.as_ref(),
        };
        let mut reading = start_jobs(&context, generation, paint, choices, edits);
        // Every row's field count, so column operations needn't wait for
        // the index pass's marks.
        reading.counts = counts.filter(|_| !converts);
        if converts {
            // So every row reads (and the edits made during the save carry
            // over) once it is current. One pass, with no lock held; the
            // save has succeeded, so it isn't cancelled.
            let _ = reading.index_job.control().wait();
            if reading.index.status() != Status::Complete {
                reading.cancel();
                return Err("the saved file couldn't be indexed".to_owned());
            }
            let column_count = reading.index.field_count_mode().unwrap_or(column_count);
            return Ok(Rebuilt {
                reading,
                column_count,
            });
        }
        Ok(Rebuilt {
            reading,
            column_count,
        })
    }

    /// Makes `new` the current reading in place of `old` (ADR-0008
    /// decision 1), under the writer lock, carrying over the edits made
    /// since `version` (during the save) as unsaved edits. Or why it
    /// couldn't (the document then keeps reading `old`).
    fn adopt(
        &self,
        old: &Arc<Reading>,
        new: Rebuilt,
        written: &Overlay,
        version: usize,
        rows: usize,
    ) -> Result<Adopted, String> {
        let one_at_a_time = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        // Nothing else replaces the reading while a save runs; checked, so
        // a bug can't drop another reading's edits.
        if !Arc::ptr_eq(&self.current(), old) {
            drop(one_at_a_time);
            new.reading.cancel();
            return Err("the document was read again while it was saved".to_owned());
        }
        let (carried, lost) = carry_over(old, written, version, &new.reading, rows);
        // The new edits carry on from the old ones' version, the carry-over
        // counted within it: a document's edit versions only ever increase,
        // and the app's change-count token for the version now still says
        // what the edits are.
        new.reading.edits.carry_on_from(old.edits.version());
        let reading = Arc::new(new.reading);
        *self.reading.write().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&reading);
        drop(one_at_a_time);
        old.cancel();
        Ok(Adopted {
            reading,
            column_count: new.column_count,
            carried,
            lost,
        })
    }
}

/// A save's new reading, not current yet ([`Document::rebuilt`]).
struct Rebuilt {
    reading: Reading,
    /// Its first screen's column count.
    column_count: usize,
}

/// A save's new reading, current now ([`Document::adopt`]).
struct Adopted {
    reading: Arc<Reading>,
    column_count: usize,
    /// The cells carried over as unsaved edits.
    carried: Vec<(usize, usize)>,
    /// Those that couldn't be: their rows weren't written.
    lost: Vec<(usize, usize)>,
}

/// For Save As (and before Save's own checks), the file already at
/// `destination`, if any, whose metadata the new file keeps, as a replace
/// does. Something other than a regular file there is refused
/// ([`SaveError::NotAFile`]), and so is a locked file.
fn existing_at(destination: &Path) -> Result<Option<Existing>, SaveError> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if !metadata.is_file() => Err(SaveError::NotAFile),
        Ok(_) => match look_afresh(destination) {
            Ok(Some(existing)) if existing.is_locked() => Err(SaveError::Locked),
            Ok(existing) => Ok(existing),
            // One Leal can't open (write-only) is replaced without its
            // metadata.
            Err(_) => Ok(None),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(SaveError::Write {
            step: "looking at the destination",
            error,
        }),
    }
}

/// Whether Leal may replace `existing`, the file at `destination`: not
/// locked, and writable by Leal (ADR-0012 decision 1).
fn may_replace(destination: &Path, existing: &Existing) -> Result<(), SaveError> {
    if existing.is_locked() {
        return Err(SaveError::Locked);
    }
    let writable = can_write(destination).map_err(|error| SaveError::Write {
        step: "checking the file can be written",
        error,
    })?;
    if writable {
        Ok(())
    } else {
        Err(SaveError::NotWritable)
    }
}

/// The cells touched in `old` since `version`, row by row: each column an
/// edit since touched, or that `written` (the edits the save wrote) had
/// edited in its row.
fn touched_cells(old: &Reading, written: &Overlay, version: usize) -> Vec<(usize, Vec<usize>)> {
    // Rows can't be inserted or deleted while a save runs (ADR-0014
    // decision 1), so the piece list now is the snapshot's: each row
    // touched is at its logical row there, which is its row in the new
    // file.
    // Nor columns (decision 1): each cell is at its logical column under
    // the snapshot's column operations, its column in the new file.
    let (touched, _, _) = old.edits.since(version);
    let ops = written.columns();
    let mut cells: Vec<(usize, Vec<usize>)> = touched
        .into_iter()
        .filter_map(|touched| {
            let row = logical_row(written, touched.id)?;
            let inserted = touched.id.inserted_index();
            let shown = |edits: &RowEdits| -> Vec<usize> {
                edits
                    .shown(ops, inserted)
                    .into_iter()
                    .map(|(column, _)| column)
                    .collect()
            };
            let mut columns: Vec<usize> = touched.edits.iter().flat_map(|e| shown(e)).collect();
            columns.extend(written.edits(touched.id).into_iter().flat_map(|e| shown(e)));
            columns.sort_unstable();
            columns.dedup();
            Some((row, columns))
        })
        .collect();
    cells.sort_unstable();
    cells
}

/// Carries the edits made in `old` since `version` (during a save) over to
/// `new`, the reading of the file the save wrote with the edits `written`:
/// each cell they touched ([`touched_cells`]) is set, by value, to what it
/// reads as in `old` now, in one change. A row past `rows` isn't in the new
/// file; its cells come back as lost. Returns the cells that are unsaved
/// edits in `new`, and those lost.
#[expect(clippy::type_complexity, reason = "two lists of cells")]
fn carry_over(
    old: &Reading,
    written: &Overlay,
    version: usize,
    new: &Reading,
    rows: usize,
) -> (Vec<(usize, usize)>, Vec<(usize, usize)>) {
    let mut cells: Vec<(usize, usize, Option<String>)> = Vec::new();
    let mut lost = Vec::new();
    for (row, columns) in touched_cells(old, written, version) {
        if row >= rows {
            lost.extend(columns.iter().map(|&column| (row, column)));
            continue;
        }
        let values: Vec<Option<String>> = Document::read_rows_of(old, row..row + 1, |view| {
            columns
                .iter()
                .map(|&column| view.value(column).map(std::borrow::Cow::into_owned))
                .collect::<Vec<_>>()
        })
        .ok()
        .and_then(|mut rows| rows.pop())
        .unwrap_or_default();
        cells.extend(
            columns
                .iter()
                .zip(values)
                .map(|(&column, value)| (row, column, value)),
        );
    }
    let targets: Vec<Target<'_>> = cells
        .iter()
        .map(|(row, column, value)| Target {
            row: *row,
            column: *column,
            value: value.as_deref(),
            expected: None,
        })
        .collect();
    let mut carried = Vec::new();
    if !targets.is_empty() {
        match Document::change_in(new, None, &targets, false) {
            Ok((_, changes)) => {
                carried.extend(changes.iter().map(|change| (change.row, change.column)));
            }
            Err(_) => lost.extend(targets.iter().map(|target| (target.row, target.column))),
        }
    }
    carried.sort_unstable();
    carried.dedup();
    (carried, lost)
}

/// The file at `destination` now, if it is the one `opened` describes (or
/// the user agreed to write over whatever is there).
fn unchanged(
    destination: &Path,
    opened: &FileIdentity,
    request: &SaveRequest,
) -> Result<Existing, SaveError> {
    let existing = look_afresh(destination)
        .map_err(|error| SaveError::Write {
            step: "looking at the file before writing",
            error,
        })?
        .ok_or(SaveError::Missing)?;
    if !request.overwrite_changed && !same_file(opened, existing.identity()) {
        return Err(SaveError::ChangedElsewhere);
    }
    Ok(existing)
}

/// What is known about the volume of `destination` (a symbolic link
/// followed): what the app said, or, if it said nothing and the file is
/// saved to the volume the old snapshot came from, what that one was, so a file on a removable drive is never
/// mapped from it (ADR-0006).
fn saved_volume(old: &Reading, destination: &Path, request: &SaveRequest) -> VolumeInfo {
    let volume = request.volume.clone();
    let unknown = volume.is_internal.is_none() && volume.is_ejectable.is_none();
    let folder = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let same_volume =
        std::fs::metadata(folder).is_ok_and(|now| now.dev() == old.source.identity().device);
    if unknown && same_volume && old.source.can_vanish() {
        VolumeInfo {
            is_internal: Some(false),
            is_ejectable: Some(true),
            ..volume
        }
    } else {
        volume
    }
}

/// `path`, or the file a symbolic link there leads to: a save replaces the
/// file, never the link.
fn resolve_link(path: &Path) -> PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
        }
        _ => path.to_owned(),
    }
}

/// The edited cells (logical row, column) on rows that aren't written,
/// those of the first `rows` logical rows being written (Save As from an
/// incomplete document, ADR-0008 decision 6): edited cells shown (not those
/// a column delete hid), and inserted rows' own and inserted cells.
fn skipped_edits(reading: &Reading, overlay: &Overlay, rows: usize) -> Vec<(usize, usize)> {
    let past = |id| logical_row(overlay, id).filter(|&row| row >= rows);
    let mut cells: Vec<(usize, usize)> = Vec::new();
    for (id, edits) in overlay.all() {
        if id.inserted_index().is_none()
            && let Some(row) = past(id)
        {
            let shown = edits.shown(overlay.columns(), None);
            cells.extend(shown.into_iter().map(|(column, _)| (row, column)));
        }
    }
    for n in overlay.inserted_rows().map(|(n, _)| n) {
        if let Some(row) = past(RowId::inserted(n)) {
            new_cells(reading, overlay, n, &mut |column, _| {
                cells.push((row, column))
            });
        }
    }
    cells.sort_unstable();
    cells.dedup();
    cells
}

/// Calls `each` with each cell of inserted row `n` that holds a value (its
/// own, an inserted column's, or an edit), by logical column.
fn new_cells(reading: &Reading, overlay: &Overlay, n: u32, each: &mut dyn FnMut(usize, &str)) {
    let Some((row, cells)) = inserted_row(overlay, n) else {
        return;
    };
    let view = RowView::inserted(&reading.parser, row, cells);
    for (column, cell) in view.filled() {
        if let ViewCell::New(value) | ViewCell::Edited(value) = cell {
            each(column, value);
        }
    }
}

/// What of `reading` a save writes: all of it, or (Save As from an
/// incomplete document) up to the end of its last trusted row, so never
/// half a row, half a character or an open quote (ADR-0008 decision 6).
fn extent_of(reading: &Reading, kind: SaveKind) -> Result<Extent<'_>, SaveError> {
    let source = reading.detection.encoding;
    let target = kind.writes(source);
    // UTF-16 files are read-only in v1 (DESIGN §1, §4.3): only Save As
    // UTF-8 writes them, converted.
    if !target.is_ascii_compatible() {
        return Err(SaveError::ReadOnly);
    }
    let complete = reading.index.status() == Status::Complete && reading.source.can_save();
    if kind == SaveKind::Save && !complete {
        return Err(SaveError::Incomplete);
    }
    let stale = reading.head_is_stale();
    let (index, rows) = reading.rows_from(&reading.index, stale);
    let end = if complete {
        usize::try_from(reading.source.len()).unwrap_or(usize::MAX)
    } else {
        // With no rows, the BOM, if it was read.
        index.rows_extent(0..rows).map_or_else(
            || {
                let bom = reading.detection.bom.len();
                bom.min(usize::try_from(reading.source.available_len()).unwrap_or(0))
            },
            |extent| extent.end,
        )
    };
    Ok(Extent {
        complete,
        index,
        rows,
        end,
        source,
        target,
    })
}

/// Every edited value written must be encodable in the encoding written
/// (F5): checked before anything is written, naming each cell that isn't,
/// by its logical row and column: edited cells shown (a deleted column's
/// aren't written), inserted rows' cells, and the cells a column insert
/// gave original rows, which are looked for (a pass over the file) only if
/// one of its values can't be encoded.
fn check_encodable(
    reading: &Reading,
    overlay: &Overlay,
    extent: &Extent<'_>,
) -> Result<(), SaveError> {
    let encoding = extent.target;
    let rows = overlay.map().rows_within(extent.rows);
    let bad = |value: &str| encode(value, encoding).is_err();
    let columns = overlay.columns();
    let new_bad = columns.ops().iter().any(|op| op.values().any(&bad));
    let mut cells: Vec<(usize, usize)> = Vec::new();
    if new_bad {
        // Every original row, as it reads, for its inserted cells too.
        let mut start = 0;
        while start < extent.rows {
            let batch = start..extent.rows.min(start + QUOTE_SCAN_ROWS);
            Document::read_physical(reading, overlay, batch.clone(), &mut |view| {
                let Some(row) = logical_row(overlay, view.id()).filter(|&row| row < rows) else {
                    return;
                };
                for (column, cell) in view.filled() {
                    if let ViewCell::New(value) | ViewCell::Edited(value) = cell
                        && bad(value)
                    {
                        cells.push((row, column));
                    }
                }
            })?;
            start = batch.end;
        }
    } else {
        for (id, edits) in overlay.all() {
            if id.inserted_index().is_some() {
                continue;
            }
            let Some(row) = logical_row(overlay, id).filter(|&row| row < rows) else {
                continue;
            };
            let shown = edits.shown(columns, None);
            cells.extend(
                shown
                    .into_iter()
                    .filter(|(_, value)| bad(value))
                    .map(|(column, _)| (row, column)),
            );
        }
    }
    for n in overlay.inserted_rows().map(|(n, _)| n) {
        let Some(row) = logical_row(overlay, RowId::inserted(n)).filter(|&row| row < rows) else {
            continue;
        };
        new_cells(reading, overlay, n, &mut |column, value| {
            if bad(value) {
                cells.push((row, column));
            }
        });
    }
    if cells.is_empty() {
        return Ok(());
    }
    cells.sort_unstable();
    Err(SaveError::Unencodable { encoding, cells })
}

/// Row `id`'s logical row in `overlay`: an original row's (`None` if it is
/// deleted), or an inserted row's.
fn logical_row(overlay: &Overlay, id: RowId) -> Option<usize> {
    let map = overlay.map();
    match id.physical() {
        Some(physical) => map.logical_of(physical).ok(),
        None => {
            let n = id.inserted_index()?;
            map.logical_of_inserted(n, overlay.inserted(n)?.gap())
        }
    }
}

pub(super) fn to_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub(super) fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
