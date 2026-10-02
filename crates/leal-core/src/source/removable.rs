//! The bytes of a file on a removable drive (ADR-0006 option C).
//!
//! The file on the drive is **never mapped**: if the drive is unplugged or
//! force-ejected, touching a mapped page kills the process (SIGBUS), while
//! an ordinary read (`pread`) just returns an error. What is read is the
//! clone on the drive if the drive's volume can clone (APFS), or the user's
//! file itself if it can't (exFAT, FAT, HFS+): the [`Origin`].
//!
//! 1. Until the copy below is complete, ranges are read with `pread`: from
//!    the internal copy if it already has them, otherwise from the drive.
//! 2. [`Removable::stream`] reads the file once, in chunks, writes each
//!    chunk to Leal's copy on the internal disk and hands it to the caller,
//!    so the index can be built in the same pass (task 1.3a).
//! 3. When the copy is complete, it is made read-only and mapped, and the
//!    clone on the drive (if any) is deleted. From then on this is an
//!    ordinary mapped copy ([`Storage::Copy`]).
//! 4. If the drive vanishes first, the source is
//!    [`Storage::Disconnected`]: the bytes already copied stay readable,
//!    the rest fail with [`ReadErrorKind::Disconnected`], and Save is
//!    refused.
//!
//! Without a clone, another program could change the user's file while it
//! is read, and the copy could then mix old and new bytes. So after every
//! read of the file (each chunk of the stream, and each `read_range` that
//! isn't served from the copy), its inode, size and modification time are
//! checked against the ones taken at open, and a short read counts as a
//! change. After a change the source reports it for good
//! ([`Removable::changed_on_disk`], [`ReadErrorKind::ChangedOnDisk`]): bytes
//! read after it are never served, the copy is never mapped, and Save is
//! refused. The app drops the rows it holds and offers Reload (task 1.9).
//!
//! **Network shares (ADR-0009)** take this path too, with three rules on
//! top ([`ShareRules`]):
//!
//! - **The share is never read on the main thread.** A read of a hard NFS
//!   mount, or of an SMB share that is reconnecting, can block for a long
//!   time. [`Removable::read_range`], which may be on the main thread, never
//!   reads the share at all: bytes not yet copied give
//!   [`ReadErrorKind::NotCopied`]. Only [`Removable::stream`] and first
//!   paint's [`Removable::read_head`] read it, and the app calls both off
//!   the main thread. A use of the share on the main thread is a bug
//!   (in an app that has said its main thread draws,
//!   [`forbid_share_use_on_main_thread`]): debug builds panic, and the test
//!   hooks count it ([`Removable::share_reads_on_main_thread`]). Even the
//!   last `close(2)` of the share's file happens on a thread of its own
//!   (`Drop`).
//! - **Network errors are retried.** `ETIMEDOUT`, `EHOSTDOWN`,
//!   `EHOSTUNREACH`, `ENETDOWN`, `ENETUNREACH`, `ECONNRESET`, `ECONNREFUSED`,
//!   `ECONNABORTED`, `ENOTCONN`, `EPIPE`, `ESHUTDOWN`, `EAGAIN` and `EIO` may
//!   pass, so a read that fails with one is tried again after each of
//!   [`NETWORK_RETRY_DELAYS`], for at most [`NETWORK_RETRY_WINDOW`] from the
//!   first failure, the tries themselves included. Then, as after any other
//!   failure on a share, the share is [`Storage::Disconnected`].
//! - **`ENOENT` or `ESTALE` is checked against the path.** If the file at
//!   the path is still the one opened, the share only lost its handle
//!   (disconnected, and it reconnects); if another file is there, the file
//!   was replaced (changed while reading); if nothing is there but its
//!   folder is, another computer deleted it ([`Storage::Deleted`]); if the
//!   folder can't be seen either, the share has gone (disconnected).
//!
//! Everything here takes `&self`, because the document shares the source
//! between threads: the stream runs on the index thread while the main
//! thread reads rows. The state that changes is in atomics, a `Mutex` and a
//! `OnceLock`.

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io;
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "test-hooks"))]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use memmap2::Mmap;

#[cfg(doc)]
use super::ReadErrorKind;
use super::error::Step;
use super::temp::TempFolder;
use super::{Chunk, FileIdentity, OpenError, ReadError, Storage, sys};

/// How long to wait before asking again whether the drive has gone, after
/// an `EIO` that doesn't look like a disconnection yet. When a drive is
/// pulled, reads can fail a moment before the kernel unmounts the volume.
const EIO_RECHECK_DELAY: Duration = Duration::from_millis(50);

/// How long to wait before each retry of a read of a network share that
/// failed with a network error (ADR-0009): five retries, doubling from
/// 100 ms, about 3 s in all. Brief enough that a share that has really gone
/// is reported soon, long enough to ride out a Wi-Fi hiccup or an SMB
/// session being re-established.
pub const NETWORK_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(400),
    Duration::from_millis(800),
    Duration::from_millis(1600),
];

/// The longest a network error is retried, from the first failure, the
/// tries themselves included: a retry is made only if it would start within
/// this. The delays add up to 3.1 s, so quick tries get all five retries; a
/// share whose every try takes a second (a timeout) gets fewer, not a
/// longer wait.
pub const NETWORK_RETRY_WINDOW: Duration = Duration::from_millis(3500);

/// Whether the app has said that its main thread draws, so that a use of a
/// network share there is a bug ([`forbid_share_use_on_main_thread`]). Off
/// for the CLI, whose main thread is its only one.
static MAIN_THREAD_DRAWS: AtomicBool = AtomicBool::new(false);

/// The app's main thread draws the window (leal-ffi says so when the app
/// makes its scheduler): from now on a network share must never be used
/// on it (ADR-0009). Debug builds panic if it is.
pub fn forbid_share_use_on_main_thread() {
    MAIN_THREAD_DRAWS.store(true, Ordering::Relaxed);
}

/// The longest a retry's wait sleeps before it looks at the cancel flag
/// again, so a cancelled pass stops promptly.
const PAUSE_SLICE: Duration = Duration::from_millis(10);

/// The rules for a file on a network share (ADR-0009), on top of a
/// removable drive's.
pub(super) struct ShareRules {
    /// The waits before each retry of a network error.
    retry_delays: Vec<Duration>,
    /// The longest the retries of one read go on, from its first failure.
    retry_window: Duration,
    /// TEST HOOK: a share that is slow or fails.
    #[cfg(any(test, feature = "test-hooks"))]
    simulated: Option<SimulatedState>,
}

impl ShareRules {
    /// A real share's rules.
    pub(super) fn new() -> Self {
        Self {
            retry_delays: NETWORK_RETRY_DELAYS.to_vec(),
            retry_window: NETWORK_RETRY_WINDOW,
            #[cfg(any(test, feature = "test-hooks"))]
            simulated: None,
        }
    }

    /// TEST HOOK: the rules of a share that behaves as `share` says.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulated(share: SimulatedShare) -> Self {
        Self {
            retry_delays: share.retry_delays.to_vec(),
            // As for the real delays: their sum, and room for the tries.
            retry_window: share.retry_window.unwrap_or_else(|| {
                share.retry_delays.iter().sum::<Duration>() + Duration::from_secs(1)
            }),
            simulated: Some(SimulatedState {
                read_delay: share.read_delay,
                failure: Mutex::new(share.failure),
                reads: AtomicUsize::new(0),
                failed: AtomicUsize::new(0),
                head_reads: AtomicUsize::new(0),
                hold_at: share.hold_at,
                released: Mutex::new(share.hold_at.is_none()),
                release: Condvar::new(),
                on_close: share.on_close,
            }),
        }
    }
}

/// TEST HOOK, not for product code: a network share that is slow, or whose
/// reads fail, for the tests of ADR-0009 without a real share. The file is
/// in an ordinary folder; [`Source::open_simulating_share`] opens it as on
/// a share that can't clone, so it is read from the user's file itself.
///
/// [`Source::open_simulating_share`]: super::Source::open_simulating_share
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy, Debug)]
pub struct SimulatedShare {
    /// How long every read of the share takes: first paint's, and each
    /// chunk of the copy (and each retry). Timed strictly, so it is the
    /// same at any QoS and in a background process.
    pub read_delay: Duration,
    /// Reads of the share that fail, if any.
    pub failure: Option<SimulatedShareFailure>,
    /// The waits before each retry of a network error, in place of
    /// [`NETWORK_RETRY_DELAYS`], so tests needn't wait seconds. The retries
    /// stop as the real ones do after [`NETWORK_RETRY_WINDOW`]: after
    /// `retry_window`, or their sum and a second.
    pub retry_delays: &'static [Duration],
    /// See `retry_delays`.
    pub retry_window: Option<Duration>,
    /// Reads of the copy that reach this byte wait until
    /// [`Source::simulated_share_release`](super::Source::simulated_share_release)
    /// or until the pass is cancelled, so a test can look at a document
    /// part-way through its copy without racing it. First paint's read isn't
    /// held.
    pub hold_at: Option<usize>,
    /// Called on each thread that closes one of the share's files (the
    /// source's, and the document's watcher's), once it has closed it, so a
    /// test can see that it isn't the caller's thread.
    pub on_close: Option<fn()>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl Default for SimulatedShare {
    /// A share that answers at once, with the real retry delays.
    fn default() -> Self {
        Self {
            read_delay: Duration::ZERO,
            failure: None,
            retry_delays: &NETWORK_RETRY_DELAYS,
            retry_window: None,
            hold_at: None,
            on_close: None,
        }
    }
}

/// TEST HOOK, not for product code: reads of a [`SimulatedShare`] that
/// fail.
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulatedShareFailure {
    /// Reads that reach this byte fail: those of bytes from `at` on.
    pub at: usize,
    /// With this error code, such as `libc::ETIMEDOUT` or `libc::ESTALE`.
    /// 0 is a short read (`UnexpectedEof`), as if the file had shrunk,
    /// whatever `fstat` says.
    pub errno: i32,
    /// This many times, then they work again. `None`: until
    /// [`Source::simulate_drive_back`](super::Source::simulate_drive_back),
    /// and the source refuses to reconnect until then, as a share that is
    /// still away would.
    pub times: Option<u32>,
    /// The `fstat` after the read fails, not the read itself.
    pub on_stat: bool,
    /// The read fills half the buffer with junk before it fails, as a
    /// partial read followed by an error does.
    pub partial: bool,
}

/// TEST HOOK: a [`SimulatedShare`] as it goes.
#[cfg(any(test, feature = "test-hooks"))]
struct SimulatedState {
    read_delay: Duration,
    /// The failure still to come; `times` counts down.
    failure: Mutex<Option<SimulatedShareFailure>>,
    /// How many reads of the share have been tried, retries included.
    reads: AtomicUsize,
    /// How many of them were made to fail.
    failed: AtomicUsize,
    /// How many first paint reads there have been (not tries).
    head_reads: AtomicUsize,
    /// See [`SimulatedShare::hold_at`].
    hold_at: Option<usize>,
    /// Whether held reads may go on.
    released: Mutex<bool>,
    release: Condvar,
    on_close: Option<fn()>,
}

/// What is read from the removable drive.
pub(super) enum Origin {
    /// Leal's clone of the file on the drive, in its recorded folder.
    Clone(TempFolder),
    /// The user's file itself, open read-only, because the drive's volume
    /// can't clone. `identity` is from `fstat` at open.
    Original {
        file: File,
        path: PathBuf,
        identity: FileIdentity,
    },
}

/// A file on a removable drive, read safely until its internal copy is
/// complete.
pub(super) struct Removable {
    // Fields are dropped in order: the map before the files.
    /// The internal copy, mapped once it is complete. Only `stream` sets it.
    map: OnceLock<Mmap>,
    /// Whether the copy may be mapped once complete: not if Leal's
    /// temporary folder is itself on a network volume (a network home
    /// folder), where a vanished share would make touching the map crash
    /// the process (task 2.0 review). Then the complete copy is read with
    /// `pread`, like the part copied so far.
    map_copy: bool,
    /// Set when the copy is complete, mapped or not.
    complete: AtomicBool,
    /// The file on the drive, until the copy is complete.
    external: Mutex<Option<External>>,
    /// Leal's copy on the internal disk, open for reading and writing. Its
    /// temporary folder is the [`Source`](super::Source)'s.
    copy: File,
    copy_path: PathBuf,
    /// The snapshot's size.
    len: usize,
    /// How many bytes, from the start, the copy holds. Bytes below this are
    /// never written again, so readers may read them from `copy`.
    copied: AtomicUsize,
    /// Set when a read finds that the drive has vanished.
    disconnected: AtomicBool,
    /// Set when the user's file (read without a clone) changed while it was
    /// being copied.
    changed: AtomicBool,
    /// Set when a read of a network share found the file deleted
    /// elsewhere (`ENOENT` or `ESTALE`, and nothing at its path).
    deleted: AtomicBool,
    /// First paint's bytes (at most 64 KB), whatever they were read from:
    /// the copy's first chunks must be the same bytes, or the file changed
    /// in between (a change the size and modification time can miss). Read
    /// from a clone they always agree, unless the drive comes back without
    /// it and the rest is read from the user's file (`reconnect`, task
    /// 2.1a). Dropped once they have been compared.
    head: Mutex<Option<Vec<u8>>>,
    /// The file is on a network share: its rules (ADR-0009). `None` for a
    /// removable drive.
    share: Option<ShareRules>,
    /// TEST HOOK: how many times the share was read on the main thread.
    #[cfg(any(test, feature = "test-hooks"))]
    main_thread_reads: AtomicUsize,
    /// Held for the whole of a stream, so only one runs at a time.
    streaming: Mutex<()>,
    chunk_len: usize,
    /// TEST HOOK: a disconnection or change to pretend happens when the
    /// copy reaches a given offset. A simulated drive stays away until
    /// `simulate_drive_back` clears it.
    #[cfg(any(test, feature = "test-hooks"))]
    fault: Mutex<Option<SimulatedFault>>,
}

/// TEST HOOK, not for product code: what to pretend happens to a file on a
/// removable drive while it is copied, so the app's tests can show the
/// "drive disconnected" and "changed while reading" states without a real
/// drive (task 1.7). See [`Source::open_simulating_fault`].
///
/// [`Source::open_simulating_fault`]: super::Source::open_simulating_fault
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulatedFault {
    /// The drive vanishes when the copy reaches byte `at`: the chunk that
    /// would read it fails with [`ReadErrorKind::Disconnected`], as after an
    /// unplug.
    ///
    /// [`ReadErrorKind::Disconnected`]: super::ReadErrorKind::Disconnected
    Disconnect {
        /// The first byte that can't be read.
        at: usize,
    },
    /// The user's file changes when the copy reaches byte `at`: the chunk
    /// that would read it fails with [`ReadErrorKind::ChangedOnDisk`], as
    /// for a drive that can't clone.
    ///
    /// [`ReadErrorKind::ChangedOnDisk`]: super::ReadErrorKind::ChangedOnDisk
    Change {
        /// The first byte read after the change.
        at: usize,
    },
}

/// The file on the removable drive.
struct External {
    // Dropped before `folder`, so the clone is closed before it is deleted.
    // Readers hold their own `Arc` for the length of one read, so the
    // descriptor can outlive this (deleting an open file is fine on Unix).
    file: Arc<File>,
    path: PathBuf,
    /// The file's `(st_dev, st_ino)`, to tell whether the path still leads
    /// to it.
    id: (u64, u64),
    /// For a clone: its folder, held so that dropping it deletes the clone,
    /// the folder and the record. `None` for the user's own file, whose
    /// size and modification time at open are in `watched` instead.
    folder: Option<TempFolder>,
    watched: Option<FileIdentity>,
}

/// What a failed read of the drive means, from its error alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Failure {
    /// The error itself says the device or volume is gone.
    Disconnected,
    /// `EIO`: a disconnection or a bad block. Only a look at the file can
    /// tell.
    Ambiguous,
    /// On a network share, a network error that may pass: try again.
    Network,
    /// On a network share, `ENOENT` or `ESTALE`: the file may have been
    /// deleted, or replaced, by another computer. `confirm_deletion` looks
    /// at the path to tell.
    Deleted,
    /// Anything else.
    Other,
}

/// A failed attempt to read the drive's file ([`Removable::attempt`]),
/// before it is classified.
enum Attempt {
    /// The read itself failed (a short read is `UnexpectedEof`).
    Read(io::Error),
    /// The read worked, but `fstat`, to check that the user's file is
    /// unchanged, failed.
    Stat(io::Error),
    /// The user's file changed (already recorded).
    Changed,
}

/// Classifies a failed read of a file on a removable drive. After a real
/// unplug, reads fail with `ENXIO` (device gone), `ENODEV`, `ENOTCONN`
/// (network or USB link down), `ESTALE` (the volume was remounted), `EBADF`
/// (the descriptor was revoked by a forced unmount) or `ENOENT`, even while
/// `fstat` and the path still look fine for a moment. A forced unmount
/// (`hdiutil detach -force`) gives `EIO`, the same code as a bad block. The
/// network errors (`ETIMEDOUT`, `EHOSTDOWN`, `EHOSTUNREACH`, `ENETDOWN`,
/// `ENETUNREACH`, `ECONNRESET`) can come from a drive behind a network or
/// Thunderbolt bridge and may be passing, so they are ambiguous too.
///
/// On a network share (`share`), every error is one of three
/// (ADR-0009): the network errors (`EIO`, which SMB gives for a timed-out
/// request, among them) are [`Failure::Network`], retried before the share
/// counts as gone; `ENOENT` and `ESTALE` are [`Failure::Deleted`], which
/// the path decides; and anything else is [`Failure::Disconnected`]: a
/// share that can't be read is treated as away, and the app keeps checking
/// for it to come back.
pub(super) fn classify(error: &io::Error, share: bool) -> Failure {
    if share {
        return match error.raw_os_error() {
            Some(
                libc::ETIMEDOUT
                | libc::EHOSTDOWN
                | libc::EHOSTUNREACH
                | libc::ENETDOWN
                | libc::ENETUNREACH
                | libc::ECONNRESET
                | libc::ECONNREFUSED
                | libc::ECONNABORTED
                | libc::ENOTCONN
                | libc::EPIPE
                | libc::ESHUTDOWN
                | libc::EAGAIN
                | libc::EIO,
            ) => Failure::Network,
            Some(libc::ENOENT | libc::ESTALE) => Failure::Deleted,
            _ => Failure::Disconnected,
        };
    }
    match error.raw_os_error() {
        Some(
            libc::ENXIO | libc::ENODEV | libc::ENOTCONN | libc::ESTALE | libc::EBADF | libc::ENOENT,
        ) => Failure::Disconnected,
        Some(
            libc::EIO
            | libc::ETIMEDOUT
            | libc::EHOSTDOWN
            | libc::EHOSTUNREACH
            | libc::ENETDOWN
            | libc::ENETUNREACH
            | libc::ECONNRESET,
        ) => Failure::Ambiguous,
        _ => Failure::Other,
    }
}

/// Whether a failed read may wait to tell a pulled drive from a bad block.
/// Only [`Removable::stream`], which runs in the background, may;
/// [`Removable::read_range`] may be on the main thread (DESIGN §3.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    Allowed,
    Never,
}

impl Removable {
    /// Takes over `origin`, what is read from the drive, and creates the
    /// (empty) internal copy in `copy_folder`. `original` is the user's
    /// file, for errors. `share` holds the rules for a file on a network
    /// share, and is `None` for a removable drive.
    pub(super) fn new(
        original: &Path,
        origin: Origin,
        copy_folder: &TempFolder,
        chunk_len: usize,
        share: Option<ShareRules>,
    ) -> Result<Self, OpenError> {
        let external = match origin {
            Origin::Clone(clone) => {
                let clone_error = |error| OpenError::new(original, Step::Clone, error);
                let path = clone.file_path();
                let file = File::open(&path).map_err(clone_error)?;
                let metadata = file.metadata().map_err(clone_error)?;
                External {
                    file: Arc::new(file),
                    path,
                    id: (metadata.dev(), metadata.ino()),
                    folder: Some(clone),
                    watched: None,
                }
            }
            Origin::Original {
                file,
                path,
                identity,
            } => External {
                file: Arc::new(file),
                path,
                id: (identity.device, identity.inode),
                folder: None,
                watched: Some(identity),
            },
        };
        // A clone's size is the snapshot's. The user's file is read only up
        // to the size it had at open.
        let len = match &external.watched {
            Some(identity) => identity.len,
            None => external
                .file
                .metadata()
                .map_err(|error| OpenError::new(original, Step::Clone, error))?
                .len(),
        };
        let len = usize::try_from(len).map_err(|_| {
            OpenError::new(
                original,
                Step::Read,
                io::Error::from(io::ErrorKind::FileTooLarge),
            )
        })?;

        let copy_path = copy_folder.file_path();
        let copy = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&copy_path)
            .map_err(|error| OpenError::new(original, Step::Copy, error))?;

        // The copy's own volume: a network home folder puts it on a share.
        let map_copy =
            !super::volume::flags(&copy).is_ok_and(|flags| !flags.local || flags.network_type);
        Ok(Self {
            map: OnceLock::new(),
            map_copy,
            complete: AtomicBool::new(false),
            external: Mutex::new(Some(external)),
            copy,
            copy_path,
            len,
            copied: AtomicUsize::new(0),
            disconnected: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            deleted: AtomicBool::new(false),
            head: Mutex::new(None),
            share,
            #[cfg(any(test, feature = "test-hooks"))]
            main_thread_reads: AtomicUsize::new(0),
            streaming: Mutex::new(()),
            // A zero chunk length would never get anywhere.
            chunk_len: chunk_len.max(1),
            #[cfg(any(test, feature = "test-hooks"))]
            fault: Mutex::new(None),
        })
    }

    /// TEST: pretend Leal's temporary folder is on a network volume, so the
    /// complete copy isn't mapped.
    #[cfg(test)]
    pub(super) fn set_copy_on_network(&mut self) {
        self.map_copy = false;
    }

    /// Whether the copy is complete: mapped, or (on a network volume) read
    /// with `pread`.
    fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// TEST HOOK: pretend `fault` happens during the copy.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn set_fault(&mut self, fault: Option<SimulatedFault>) {
        *self.fault.get_mut().unwrap_or_else(PoisonError::into_inner) = fault;
    }

    /// TEST HOOK: the simulated fault's error, if a read of the drive up to
    /// `end` reaches it. It sets the state a real fault would.
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulated_fault(&self, end: usize) -> Option<ReadError> {
        let fault = *self.fault.lock().unwrap_or_else(PoisonError::into_inner);
        match fault? {
            SimulatedFault::Disconnect { at } if end > at => {
                self.disconnected.store(true, Ordering::Release);
                Some(ReadError::disconnected(io::Error::from_raw_os_error(
                    libc::ENXIO,
                )))
            }
            SimulatedFault::Change { at } if end > at => {
                self.changed.store(true, Ordering::Release);
                Some(ReadError::changed_on_disk())
            }
            SimulatedFault::Disconnect { .. } | SimulatedFault::Change { .. } => None,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The whole snapshot, once the internal copy is complete and mapped.
    pub(super) fn as_slice(&self) -> Option<&[u8]> {
        self.map.get().map(|map| &map[..])
    }

    pub(super) fn storage(&self) -> Storage {
        if self.is_complete() {
            Storage::Copy
        } else if self.deleted.load(Ordering::Acquire) {
            Storage::Deleted
        } else if self.disconnected.load(Ordering::Acquire) {
            Storage::Disconnected
        } else {
            Storage::Reading
        }
    }

    /// Whether the user's file changed while it was being copied (only
    /// possible without a clone).
    pub(super) fn changed_on_disk(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }

    /// Whether the file is on a network share (ADR-0009).
    pub(super) fn is_share(&self) -> bool {
        self.share.is_some()
    }

    /// How many bytes, from the start, can be read: all of them, unless the
    /// drive vanished, or the file on a share was deleted, before they were
    /// copied.
    pub(super) fn available_len(&self) -> usize {
        if matches!(self.storage(), Storage::Disconnected | Storage::Deleted) {
            self.copied.load(Ordering::Acquire)
        } else {
            self.len
        }
    }

    /// The watcher saw the user's file written to (task 1.9). Without a
    /// clone, what is still to be read from it is a different version, so
    /// this is a change, as if a read's own check had found it: nothing
    /// more is read from the file and Save is refused. A clone is a
    /// snapshot, and a complete copy is too, so for them this does nothing.
    pub(super) fn note_original_written(&self) {
        if self.is_complete() {
            return;
        }
        let reads_the_original = self
            .lock_external()
            .as_ref()
            .is_some_and(|external| external.watched.is_some());
        if reads_the_original {
            self.changed.store(true, Ordering::Release);
        }
    }

    /// Where the user's file is now, as the watcher follows it (task 1.9):
    /// for a file read without a clone, the path a look at it uses
    /// (`confirm_deletion`). A clone's path is Leal's own, and stays.
    pub(super) fn note_original_path(&self, path: &Path) {
        if let Some(external) = self.lock_external().as_mut()
            && external.watched.is_some()
            && external.path != path
        {
            external.path = path.to_owned();
        }
    }

    /// The drive is back (task 1.9): if the source was disconnected before
    /// its copy was complete, read the rest from the drive again. `original`
    /// is where the user's file is now, and `opened` what it was when it
    /// was opened. Returns whether the source is reading again.
    ///
    /// It reopens, in order:
    /// 1. **The clone on the drive**, if it survived the disconnection and
    ///    is the same file (inode and size). It is a snapshot, so it serves
    ///    the rest of the bytes as they were when the file was opened.
    /// 2. **The user's file**, if its inode, size and modification time are
    ///    still the ones it had when opened (its device number changes with
    ///    each mount, so isn't compared). From then on it is read as on a
    ///    drive that can't clone: checked after every read, and what is
    ///    left to copy of the first 64 KB against first paint's bytes, read
    ///    from the clone (task 2.1a).
    ///
    /// Nothing is reopened after the file changed while it was read, or
    /// was deleted on its share, and nothing is needed once the copy is
    /// complete.
    pub(super) fn reconnect(&self, original: &Path, opened: &FileIdentity) -> bool {
        if self.share.is_some() {
            self.note_share_use();
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if self.simulated_away() {
            // A simulated drive is away until the test brings it back.
            return false;
        }
        if !self.disconnected.load(Ordering::Acquire)
            || self.changed_on_disk()
            || self.deleted.load(Ordering::Acquire)
            || self.is_complete()
        {
            return false;
        }
        // No stream is running: it stopped when the drive went. Holding its
        // lock keeps a new one from starting until this is done.
        let _streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut external = self.lock_external();
        let Some(external) = external.as_mut() else {
            return false;
        };
        let reopened = if external.folder.is_some()
            && let Ok(clone) = File::open(&external.path)
            && let Ok(now) = clone.metadata()
            && now.ino() == external.id.1
            && usize::try_from(now.len()) == Ok(self.len)
        {
            external.file = Arc::new(clone);
            external.id = (now.dev(), now.ino());
            true
        } else if let Ok(file) = File::open(original)
            && let Ok(now) = file.metadata()
            && now.ino() == opened.inode
            && now.len() == opened.len
            && now.modified().ok() == opened.modified
        {
            external.file = Arc::new(file);
            external.path = original.to_owned();
            external.id = (now.dev(), now.ino());
            external.watched = Some(FileIdentity {
                device: now.dev(),
                ..*opened
            });
            true
        } else {
            false
        };
        if reopened {
            self.disconnected.store(false, Ordering::Release);
        }
        reopened
    }

    /// TEST HOOK: whether a simulated drive or share is away until the test
    /// brings it back.
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulated_away(&self) -> bool {
        let drive_away = matches!(
            *self.fault.lock().unwrap_or_else(PoisonError::into_inner),
            Some(SimulatedFault::Disconnect { .. })
        );
        let share_away = self
            .share
            .as_ref()
            .and_then(|share| share.simulated.as_ref())
            .is_some_and(|simulated| {
                simulated
                    .failure
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some_and(|failure| failure.times.is_none())
            });
        drive_away || share_away
    }

    /// TEST HOOK: the simulated drive is plugged back in, or the simulated
    /// share answers again. Nothing changes until `reconnect`, as for a
    /// real drive.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulate_drive_back(&self) {
        *self.fault.lock().unwrap_or_else(PoisonError::into_inner) = None;
        if let Some(simulated) = self.simulated() {
            *simulated
                .failure
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
    }

    /// TEST HOOK: Leal's clone on the drive is lost while the drive is
    /// away: it is deleted, so `reconnect` can't reopen it and falls back
    /// to the user's file. Returns whether there was a clone to lose.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulate_clone_lost(&self) -> bool {
        let external = self.lock_external();
        let Some(External {
            path,
            folder: Some(_),
            ..
        }) = external.as_ref()
        else {
            return false;
        };
        fs::remove_file(path).is_ok()
    }

    /// TEST HOOK: the simulated share's close callback, if any.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn share_close_hook(&self) -> Option<fn()> {
        self.simulated()?.on_close
    }

    /// TEST HOOK: the simulated share's state, if this is one.
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulated(&self) -> Option<&SimulatedState> {
        self.share.as_ref()?.simulated.as_ref()
    }

    /// TEST HOOK: reads of the simulated share held at its `hold_at` go on.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulated_share_release(&self) {
        if let Some(simulated) = self.simulated() {
            *simulated
                .released
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = true;
            simulated.release.notify_all();
        }
    }

    /// TEST HOOK: how many first paint reads of the simulated share there
    /// have been.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulated_head_reads(&self) -> usize {
        self.simulated()
            .map_or(0, |simulated| simulated.head_reads.load(Ordering::Relaxed))
    }

    /// TEST HOOK: how many times the share was read on the main thread
    /// (ADR-0009 says never).
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn share_reads_on_main_thread(&self) -> usize {
        self.main_thread_reads.load(Ordering::Relaxed)
    }

    /// TEST HOOK: how many reads of a simulated share have been tried
    /// (retries included), and how many of them were made to fail.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn simulated_share_reads(&self) -> (usize, usize) {
        self.share
            .as_ref()
            .and_then(|share| share.simulated.as_ref())
            .map_or((0, 0), |simulated| {
                (
                    simulated.reads.load(Ordering::Relaxed),
                    simulated.failed.load(Ordering::Relaxed),
                )
            })
    }

    /// The bytes in `range`, which the caller has already clamped to the
    /// file. Never waits (it may be on the main thread). On a network
    /// share, bytes that aren't copied yet aren't read at all: they give
    /// [`ReadErrorKind::NotCopied`] (ADR-0009).
    pub(super) fn read_range(&self, range: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
        self.read(range, Reader::Range)
    }

    /// The first `len` bytes (or the whole file, if it is shorter), for
    /// first paint. On a removable drive this is what
    /// [`read_range`](Self::read_range) gives. On a network share it reads
    /// the share, which `read_range` never does: it may block for as long
    /// as the share takes to answer, and it retries network errors (for at
    /// most [`NETWORK_RETRY_WINDOW`]), so it must not be on the main thread
    /// (ADR-0009). The bytes are kept until the copy has the same ones, to
    /// check that the file didn't change in between: read from the user's
    /// file itself (no clone), or from a clone that may be lost before the
    /// copy is (`reconnect`).
    pub(super) fn read_head(&self, len: usize) -> Result<Cow<'_, [u8]>, ReadError> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(simulated) = self.simulated() {
            simulated.head_reads.fetch_add(1, Ordering::Relaxed);
        }
        self.read(0..len.min(self.len), Reader::Head)
    }

    /// [`read_range`](Self::read_range) and [`read_head`](Self::read_head).
    fn read(&self, range: Range<usize>, reader: Reader) -> Result<Cow<'_, [u8]>, ReadError> {
        if let Some(map) = self.map.get() {
            return Ok(Cow::Borrowed(&map[range]));
        }
        if range.is_empty() {
            return Ok(Cow::Borrowed(&[]));
        }
        if range.end <= self.copied.load(Ordering::Acquire) {
            return read_at(&self.copy, range)
                .map(Cow::Owned)
                .map_err(ReadError::other);
        }
        if self.deleted.load(Ordering::Acquire) {
            return Err(ReadError::already_deleted());
        }
        if self.disconnected.load(Ordering::Acquire) {
            return Err(ReadError::already_disconnected());
        }
        if self.changed_on_disk() {
            return Err(ReadError::changed_on_disk());
        }
        // A share is read only by first paint and the stream, never by a
        // reader that may be on the main thread (ADR-0009).
        if self.share.is_some() && reader == Reader::Range {
            return Err(ReadError::not_copied());
        }
        let Some(external) = self.external_file() else {
            // The copy was completed and mapped since the first check.
            return match self.map.get() {
                Some(map) => Ok(Cow::Borrowed(&map[range])),
                None => Err(ReadError::other(io::Error::other(
                    "the drive's file was dropped before its copy was mapped",
                ))),
            };
        };
        let mut bytes = vec![0; range.len()];
        // `read_range` never waits. First paint on a share is off the main
        // thread, and may wait for the share as the stream does; on a drive
        // it may be on the main thread (tests open files there).
        let wait = if reader == Reader::Head && self.share.is_some() {
            Wait::Allowed
        } else {
            Wait::Never
        };
        // Nothing cancels first paint: no open can be cancelled (the app's
        // `NSDocumentController` has no way to), so the retry window bounds
        // it instead. `read_range` never retries.
        let never = AtomicBool::new(false);
        // Without a clone, bytes read after a change are never returned.
        self.read_origin(&external, &mut bytes, range.start, wait, &never)?;
        // Kept whatever they were read from, the clone included: a drive
        // that comes back without its clone is read from the user's file,
        // and the copy's chunks are checked against these (task 2.1a).
        if reader == Reader::Head && range.start == 0 {
            *self.head.lock().unwrap_or_else(PoisonError::into_inner) = Some(bytes.clone());
        }
        Ok(Cow::Owned(bytes))
    }

    /// Whether the drive's file is the user's file itself (no clone).
    fn reads_the_original(&self) -> bool {
        self.lock_external()
            .as_ref()
            .is_some_and(|external| external.watched.is_some())
    }

    /// The copy's chunk at `offset` must agree with first paint's bytes,
    /// where it overlaps them; otherwise the user's file changed between
    /// first paint and the copy, which a same-size change with its time
    /// kept can hide from `fstat`. That needs the user's file to be read
    /// (no clone, or a drive back without its clone); a clone's chunks
    /// always agree. The bytes are let go once the copy is past them.
    fn check_against_head(&self, chunk: &[u8], offset: usize) -> Result<(), ReadError> {
        let mut head = self.head.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(bytes) = head.as_ref() else {
            return Ok(());
        };
        let end = bytes.len().min(offset + chunk.len());
        if offset < end && chunk[..end - offset] != bytes[offset..end] {
            *head = None;
            self.changed.store(true, Ordering::Release);
            return Err(ReadError::changed_on_disk());
        }
        if offset + chunk.len() >= bytes.len() {
            *head = None;
        }
        Ok(())
    }

    /// Reads the whole file once, in order, calling `on_chunk` with each
    /// chunk, and copies it to the internal disk on the way. When the copy
    /// is complete, maps it and drops the file on the drive.
    ///
    /// Every pass starts at offset 0. Chunks already copied (by an earlier,
    /// cancelled pass) are read back from the internal copy; once the copy
    /// is mapped, chunks come from the map. Once the user's file is known to
    /// have changed, every pass fails straight away.
    pub(super) fn stream(
        &self,
        cancel: &AtomicBool,
        mut on_chunk: impl FnMut(Chunk<'_>),
    ) -> Result<(), ReadError> {
        let _streaming = self
            .streaming
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(map) = self.map.get() {
            return super::stream_slice(map, self.chunk_len, cancel, on_chunk);
        }
        if self.changed_on_disk() {
            return Err(ReadError::changed_on_disk());
        }

        let mut buffer = vec![0; self.chunk_len.min(self.len)];
        let mut offset = 0;
        while offset < self.len {
            if cancel.load(Ordering::Relaxed) {
                return Err(ReadError::cancelled());
            }
            let end = self.len.min(offset + self.chunk_len);
            let chunk = &mut buffer[..end - offset];
            // Every pass uses the same chunks, so the copied part always
            // ends on a chunk boundary.
            if end <= self.copied.load(Ordering::Acquire) {
                self.copy
                    .read_exact_at(chunk, to_u64(offset))
                    .map_err(ReadError::other)?;
            } else {
                self.copy_chunk(chunk, offset, cancel)?;
                self.copied.store(end, Ordering::Release);
            }
            on_chunk(Chunk {
                offset,
                bytes: chunk,
            });
            offset = end;
        }
        self.switch_to_map()
    }

    /// Reads the chunk at `offset` from the drive into `chunk`, checks that
    /// the user's file (if there is no clone) hasn't changed, and writes the
    /// chunk to the internal copy. A chunk read after a change is neither
    /// copied nor delivered. On a share, a network error is retried until
    /// `cancel` is set.
    fn copy_chunk(
        &self,
        chunk: &mut [u8],
        offset: usize,
        cancel: &AtomicBool,
    ) -> Result<(), ReadError> {
        if self.deleted.load(Ordering::Acquire) {
            return Err(ReadError::already_deleted());
        }
        if self.disconnected.load(Ordering::Acquire) {
            return Err(ReadError::already_disconnected());
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(error) = self.simulated_fault(offset + chunk.len()) {
            return Err(error);
        }
        let Some(external) = self.external_file() else {
            return Err(ReadError::other(io::Error::other(
                "the drive's file was dropped before its copy was complete",
            )));
        };
        #[cfg(any(test, feature = "test-hooks"))]
        self.simulate_share_hold(offset + chunk.len(), cancel)?;
        self.read_origin(&external, chunk, offset, Wait::Allowed, cancel)?;
        self.check_against_head(chunk, offset)?;
        self.copy
            .write_all_at(chunk, to_u64(offset))
            .map_err(ReadError::other)
    }

    /// Reads `buf.len()` bytes at `offset` from the drive's file `file` into
    /// `buf` and, for the user's own file (no clone), checks that it hasn't
    /// changed. On a network share, a network error is tried again after
    /// each of the share's retry delays, within its retry window from the
    /// first failure and unless `cancel` is set meanwhile; then the share
    /// counts as disconnected. `ENOENT` or `ESTALE` is decided by the path
    /// (`confirm_deletion`). A failure on a removable drive is classified as
    /// before ([`classify`]).
    fn read_origin(
        &self,
        file: &File,
        buf: &mut [u8],
        offset: usize,
        wait: Wait,
        cancel: &AtomicBool,
    ) -> Result<(), ReadError> {
        let share = self.share.as_ref();
        if share.is_some() {
            self.note_share_use();
        }
        let mut retries = share
            .map_or(&[][..], |share| share.retry_delays.as_slice())
            .iter();
        let window = share.map_or(Duration::ZERO, |share| share.retry_window);
        let mut first_failure: Option<Instant> = None;
        loop {
            let (error, from_stat) = match self.attempt(file, buf, offset) {
                Ok(()) => return Ok(()),
                Err(Attempt::Changed) => return Err(ReadError::changed_on_disk()),
                Err(Attempt::Read(error)) => (error, false),
                Err(Attempt::Stat(error)) => (error, true),
            };
            let failure = match classify(&error, share.is_some()) {
                failure @ (Failure::Network | Failure::Deleted) => failure,
                // `fstat` itself failed: the descriptor was revoked.
                _ if from_stat => Failure::Disconnected,
                failure => failure,
            };
            match failure {
                // A reader that mustn't wait doesn't retry, and one failure
                // there decides nothing. (No such reader reads a share: only
                // first paint and the stream do, and both may wait.)
                Failure::Network if wait == Wait::Never => return Err(ReadError::other(error)),
                Failure::Network => {
                    let since = first_failure.get_or_insert_with(Instant::now).elapsed();
                    match retries.next() {
                        Some(&delay) if since + delay <= window => {
                            if !pause(delay, cancel) {
                                return Err(ReadError::cancelled());
                            }
                        }
                        _ => {
                            // Still failing after the last retry, or the
                            // window is up: the share is as good as gone,
                            // and gives the same state as an unplugged drive
                            // (ADR-0009).
                            self.disconnected.store(true, Ordering::Release);
                            return Err(ReadError::disconnected(error));
                        }
                    }
                }
                Failure::Deleted => return Err(self.confirm_deletion(error)),
                failure => return Err(self.failed_with(error, failure, wait)),
            }
        }
    }

    /// One read of `buf.len()` bytes at `offset`, then the check that the
    /// user's file is unchanged. Reads never go past the size the file had
    /// at open, so a short read of the user's own file means it shrank: a
    /// change, whatever `fstat` says (a share's client may still report the
    /// old size). For a clone, which can't shrink, it is a read error.
    fn attempt(&self, file: &File, buf: &mut [u8], offset: usize) -> Result<(), Attempt> {
        #[cfg(any(test, feature = "test-hooks"))]
        let read = match self.simulate_share_read(offset, buf, false) {
            Some(error) => Err(error),
            None => file.read_exact_at(buf, to_u64(offset)),
        };
        #[cfg(not(any(test, feature = "test-hooks")))]
        let read = file.read_exact_at(buf, to_u64(offset));
        if let Err(error) = read {
            if error.kind() == io::ErrorKind::UnexpectedEof && self.reads_the_original() {
                self.changed.store(true, Ordering::Release);
                return Err(Attempt::Changed);
            }
            return Err(Attempt::Read(error));
        }
        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(error) = self.simulate_share_read(offset, buf, true) {
            return Err(Attempt::Stat(error));
        }
        self.check()
    }

    /// `ENOENT` or `ESTALE` from the share: look at the path itself, in the
    /// background (ADR-0009, task 2.0 review). The share's own errno isn't
    /// proof of a deletion: an NFS server that restarted gives `ESTALE` for
    /// a file that is still there.
    /// - The file at the path is the one opened (same inode, size and
    ///   modification time): only the handle went stale. Disconnected, so
    ///   the app's check reconnects it.
    /// - Another file is there: the file was replaced. Changed while
    ///   reading, so the app offers Reload.
    /// - Nothing is there, but its folder is, on the same volume: another
    ///   computer deleted it.
    /// - The folder can't be seen either, or it is on another volume (an
    ///   empty mount point), or the look fails: the share has gone.
    ///   Disconnected.
    ///
    /// The path is where the user's file is now (`note_original_path`, from
    /// the watcher), not where it was opened.
    fn confirm_deletion(&self, error: io::Error) -> ReadError {
        let (path, opened) = {
            let external = self.lock_external();
            let Some(external) = external.as_ref() else {
                return self.disconnect(error);
            };
            (external.path.clone(), external.watched)
        };
        match fs::metadata(&path) {
            Ok(now) => {
                let same = match opened {
                    Some(opened) => same_contents(&opened, &now),
                    None => false,
                };
                if same {
                    self.disconnect(error)
                } else {
                    self.changed.store(true, Ordering::Release);
                    ReadError::changed_on_disk()
                }
            }
            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {
                // The folder must be there, and on the share: an unmounted
                // share leaves its empty mount point, on the boot volume,
                // where a file at the share's root was (task 2.0 review).
                let folder_there = path.parent().is_some_and(|folder| {
                    fs::metadata(folder).is_ok_and(|folder| {
                        opened.is_none_or(|opened| folder.dev() == opened.device)
                    })
                });
                if folder_there {
                    self.deleted.store(true, Ordering::Release);
                    ReadError::deleted(error)
                } else {
                    self.disconnect(error)
                }
            }
            Err(_) => self.disconnect(error),
        }
    }

    /// The share or drive is away: `error` is what the read reported.
    fn disconnect(&self, error: io::Error) -> ReadError {
        self.disconnected.store(true, Ordering::Release);
        ReadError::disconnected(error)
    }

    /// The share is about to be used (read, `fstat`ed, reopened). It must
    /// not be on the main thread of an app (ADR-0009): debug builds panic,
    /// and the test hooks count it.
    #[cfg_attr(
        not(any(test, feature = "test-hooks")),
        expect(clippy::unused_self, reason = "only the test hooks count with it")
    )]
    pub(super) fn note_share_use(&self) {
        if !on_main_thread() {
            return;
        }
        #[cfg(any(test, feature = "test-hooks"))]
        self.main_thread_reads.fetch_add(1, Ordering::Relaxed);
        share_used_on_main_thread();
    }

    /// TEST HOOK: reads of the copy reaching `end` wait while the simulated
    /// share holds them, unless `cancel` is set meanwhile (looked at every
    /// `PAUSE_SLICE`).
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulate_share_hold(&self, end: usize, cancel: &AtomicBool) -> Result<(), ReadError> {
        let Some(simulated) = self.simulated() else {
            return Ok(());
        };
        if simulated.hold_at.is_none_or(|at| end <= at) {
            return Ok(());
        }
        let mut released = simulated
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while !*released {
            if cancel.load(Ordering::Relaxed) {
                return Err(ReadError::cancelled());
            }
            released = simulated
                .release
                .wait_timeout(released, PAUSE_SLICE)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    /// TEST HOOK: a read of the simulated share at `offset` into `buf`
    /// (`stat`: the `fstat` after it): waits its delay, then fails if its
    /// failure says so, after filling half of `buf` with junk if it is
    /// partial.
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulate_share_read(&self, offset: usize, buf: &mut [u8], stat: bool) -> Option<io::Error> {
        let simulated = self.simulated()?;
        let len = buf.len();
        if !stat {
            simulated.reads.fetch_add(1, Ordering::Relaxed);
            if !simulated.read_delay.is_zero() {
                // On a strict timer: `thread::sleep` can overshoot by up
                // to 100 ms in a background process (`sys::sleep_strictly`).
                if sys::sleep_strictly(simulated.read_delay).is_err() {
                    std::thread::sleep(simulated.read_delay);
                }
            }
        }
        let mut failure = simulated
            .failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let current = (*failure)?;
        if offset + len <= current.at || current.on_stat != stat {
            return None;
        }
        match current.times {
            Some(0) => {
                *failure = None;
                return None;
            }
            Some(times) => {
                *failure = Some(SimulatedShareFailure {
                    times: Some(times - 1),
                    ..current
                });
            }
            None => {}
        }
        simulated.failed.fetch_add(1, Ordering::Relaxed);
        if current.partial {
            buf[..len / 2].fill(0xEE);
        }
        Some(if current.errno == 0 {
            io::Error::from(io::ErrorKind::UnexpectedEof)
        } else {
            io::Error::from_raw_os_error(current.errno)
        })
    }

    /// The copy is complete: make it read-only, map it, and drop the file on
    /// the drive (deleting the clone, its folder and its record, if it is
    /// one). The last chunk's change check was the last read of the file.
    fn switch_to_map(&self) -> Result<(), ReadError> {
        if self.is_complete() {
            // A copy on a network volume, already complete and not mapped.
            return Ok(());
        }
        fs::set_permissions(&self.copy_path, Permissions::from_mode(0o400))
            .map_err(ReadError::other)?;
        if self.map_copy {
            let map = sys::map_read_only(&File::open(&self.copy_path).map_err(ReadError::other)?)
                .map_err(ReadError::other)?;
            // Only `stream` sets the map, and only one stream runs at a time,
            // so it isn't set yet.
            let _ = self.map.set(map);
        }
        self.complete.store(true, Ordering::Release);
        // Taken out first, so the lock isn't held while the files are
        // deleted.
        let external = self.lock_external().take();
        drop(external);
        Ok(())
    }

    /// For the user's own file (no clone): `fstat` it and compare its
    /// inode, size and modification time with the ones at open. A change
    /// sets `changed` for good and gives [`Attempt::Changed`]; a failed
    /// `fstat` gives [`Attempt::Stat`]. A clone can't change, so it is
    /// always `Ok`.
    fn check(&self) -> Result<(), Attempt> {
        if self.changed_on_disk() {
            return Err(Attempt::Changed);
        }
        let external = self.lock_external();
        let Some(External {
            file,
            watched: Some(watched),
            ..
        }) = external.as_ref()
        else {
            return Ok(());
        };
        match file.metadata() {
            Ok(now) if same_contents(watched, &now) => Ok(()),
            Ok(_) => {
                self.changed.store(true, Ordering::Release);
                Err(Attempt::Changed)
            }
            Err(error) => Err(Attempt::Stat(error)),
        }
    }

    /// The drive's descriptor, for one read, or `None` once it is dropped.
    fn external_file(&self) -> Option<Arc<File>> {
        self.lock_external()
            .as_ref()
            .map(|external| Arc::clone(&external.file))
    }

    fn lock_external(&self) -> MutexGuard<'_, Option<External>> {
        // Nothing that can panic runs while the lock is held, and the value
        // is only ever replaced whole, so a poisoned lock's value is fine.
        self.external.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Turns a failed read of the drive into an error, noting a vanished
    /// drive.
    fn failed_with(&self, error: io::Error, failure: Failure, wait: Wait) -> ReadError {
        let disconnected = match failure {
            Failure::Disconnected => true,
            // In the background, the look at the file is asked twice: a
            // pulled drive can fail reads a moment before its volume is
            // unmounted. `read_range` doesn't wait; a stream (or the next
            // read) catches the disconnection soon after.
            Failure::Ambiguous => {
                self.drive_vanished()
                    || (wait == Wait::Allowed && {
                        std::thread::sleep(EIO_RECHECK_DELAY);
                        self.drive_vanished()
                    })
            }
            // `read_origin` handles a share's own kinds before this.
            Failure::Network | Failure::Deleted | Failure::Other => false,
        };
        if disconnected {
            self.disconnected.store(true, Ordering::Release);
            ReadError::disconnected(error)
        } else {
            ReadError::other(error)
        }
    }

    /// Whether the drive has gone, after an `EIO`: either the kernel
    /// revoked the open descriptor (a forced unmount does that, and then
    /// `fstat` fails with `EBADF`), or, for a clone, its path no longer
    /// leads to it (the volume isn't mounted there any more). The user's own
    /// file may simply have been renamed, so only its descriptor is asked.
    /// A read error with neither, such as a bad block, isn't a
    /// disconnection.
    fn drive_vanished(&self) -> bool {
        let guard = self.lock_external();
        let Some(external) = guard.as_ref() else {
            return false;
        };
        if external.file.metadata().is_err() {
            return true;
        }
        external.folder.is_some()
            && fs::metadata(&external.path).map_or(true, |metadata| {
                (metadata.dev(), metadata.ino()) != external.id
            })
    }

    /// The clone on the drive, or the user's file, while there is one.
    #[cfg(test)]
    pub(super) fn external_clone(&self) -> Option<PathBuf> {
        self.lock_external()
            .as_ref()
            .map(|external| external.path.clone())
    }

    /// How many of first paint's bytes are kept for the copy to check.
    #[cfg(test)]
    pub(super) fn kept_head_len(&self) -> usize {
        self.head
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(0, Vec::len)
    }

    /// Lets go of the clone on the drive as a crash would.
    #[cfg(test)]
    pub(super) fn abandon_external_clone(&self) {
        if let Some(External {
            folder: Some(folder),
            ..
        }) = self.lock_external().take()
        {
            folder.abandon();
        }
    }
}

impl Drop for Removable {
    /// The last `close(2)` of a file on a network share can block, like any
    /// other use of a share that has stopped answering, so it happens on a
    /// thread of its own, never on the thread dropping the document (task
    /// 2.0 review). A file on a removable drive closes here, as before.
    fn drop(&mut self) {
        #[cfg(any(test, feature = "test-hooks"))]
        let on_close = self.share_close_hook();
        if self.share.is_none() {
            return;
        }
        let Some(external) = self
            .external
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let close = move || {
            drop(external);
            #[cfg(any(test, feature = "test-hooks"))]
            if let Some(on_close) = on_close {
                on_close();
            }
        };
        // If the thread can't be started, the closure (and the file) is
        // dropped here instead.
        let _ = std::thread::Builder::new()
            .name("leal-share-close".to_owned())
            .spawn(close);
    }
}

/// A network share was used on the main thread of an app: a bug in Leal,
/// not in the user's file (ADR-0009). A debug assertion: release builds go
/// on, rather than fail the open.
pub(super) fn share_used_on_main_thread() {
    if cfg!(debug_assertions) {
        panic!(
            "a network share was used on the main thread, where a hung share would freeze the app (ADR-0009)"
        );
    }
}

/// Whether the file `now` describes still has the size and modification
/// time it had at open (`then`), and is the same file.
fn same_contents(then: &FileIdentity, now: &fs::Metadata) -> bool {
    now.ino() == then.inode && now.len() == then.len && now.modified().ok() == then.modified
}

/// Which reader asks [`Removable::read`] for bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// [`Removable::read_range`]: perhaps on the main thread.
    Range,
    /// [`Removable::read_head`]: first paint, off the main thread.
    Head,
}

/// Waits `delay`, unless `cancel` is set first. Returns whether it waited
/// the whole time.
fn pause(delay: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(PAUSE_SLICE));
    }
}

#[cfg(test)]
thread_local! {
    /// TEST HOOK: this thread counts as the main thread, so a test (which
    /// never runs on the process's main thread) can check what happens
    /// there.
    pub(crate) static PRETEND_MAIN_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether this is the main thread of an app that draws on it
/// ([`forbid_share_use_on_main_thread`]), or, in a test, pretends to be.
pub(super) fn on_main_thread() -> bool {
    #[cfg(test)]
    if PRETEND_MAIN_THREAD.with(std::cell::Cell::get) {
        return true;
    }
    MAIN_THREAD_DRAWS.load(Ordering::Relaxed) && sys::is_main_thread()
}

/// Reads exactly `range` of `file` with `pread`.
fn read_at(file: &File, range: Range<usize>) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; range.len()];
    file.read_exact_at(&mut bytes, to_u64(range.start))?;
    Ok(bytes)
}

/// An offset as `pread` and `pwrite` take it. Lossless on Apple's 64-bit
/// platforms.
fn to_u64(offset: usize) -> u64 {
    u64::try_from(offset).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(errno: i32) -> Failure {
        classify(&io::Error::from_raw_os_error(errno), false)
    }

    fn share_failure(errno: i32) -> Failure {
        classify(&io::Error::from_raw_os_error(errno), true)
    }

    #[test]
    fn errors_that_mean_the_drive_is_gone_are_disconnections() {
        for errno in [
            libc::ENXIO,
            libc::ENODEV,
            libc::ENOTCONN,
            libc::ESTALE,
            libc::EBADF,
            libc::ENOENT,
        ] {
            assert_eq!(failure(errno), Failure::Disconnected, "errno {errno}");
        }
    }

    #[test]
    fn eio_and_network_errors_need_a_look_at_the_file() {
        for errno in [
            libc::EIO,
            libc::ETIMEDOUT,
            libc::EHOSTDOWN,
            libc::EHOSTUNREACH,
            libc::ENETDOWN,
            libc::ENETUNREACH,
            libc::ECONNRESET,
        ] {
            assert_eq!(failure(errno), Failure::Ambiguous, "errno {errno}");
        }
    }

    #[test]
    fn other_errors_are_not_disconnections() {
        for errno in [libc::EACCES, libc::EPERM, libc::ENOSPC, libc::EINVAL] {
            assert_eq!(failure(errno), Failure::Other, "errno {errno}");
        }
        let eof = io::Error::from(io::ErrorKind::UnexpectedEof);
        assert_eq!(classify(&eof, false), Failure::Other);
    }

    /// On a share (ADR-0009, task 2.0 review): the network errors and `EIO`
    /// are retried, `ENOENT` and `ESTALE` are decided by the path
    /// (`Deleted`, for `confirm_deletion`), and anything else disconnects
    /// the share.
    #[test]
    fn a_shares_errors_are_network_errors_or_a_deletion() {
        for errno in [
            libc::ETIMEDOUT,
            libc::EHOSTDOWN,
            libc::EHOSTUNREACH,
            libc::ENETDOWN,
            libc::ENETUNREACH,
            libc::ECONNRESET,
        ] {
            assert_eq!(share_failure(errno), Failure::Network, "errno {errno}");
        }
        // Also retried on a share (task 2.0 review): SMB's timed-out request
        // (`EIO`), a refused or dropped connection, a broken pipe.
        for errno in [
            libc::EIO,
            libc::ENOTCONN,
            libc::ECONNREFUSED,
            libc::ECONNABORTED,
            libc::EPIPE,
            libc::ESHUTDOWN,
            libc::EAGAIN,
        ] {
            assert_eq!(share_failure(errno), Failure::Network, "errno {errno}");
        }
        for errno in [libc::ENOENT, libc::ESTALE] {
            assert_eq!(share_failure(errno), Failure::Deleted, "errno {errno}");
        }
        // Anything else on a share: away, not an ordinary read error.
        for errno in [
            libc::ENXIO,
            libc::ENODEV,
            libc::EBADF,
            libc::EACCES,
            libc::EPERM,
            libc::EINVAL,
        ] {
            assert_eq!(share_failure(errno), Failure::Disconnected, "errno {errno}");
        }
        let eof = io::Error::from(io::ErrorKind::UnexpectedEof);
        assert_eq!(classify(&eof, true), Failure::Disconnected);
    }

    #[test]
    fn a_pause_stops_when_cancelled() {
        let started = Instant::now();
        assert!(pause(Duration::from_millis(20), &AtomicBool::new(false)));
        assert!(started.elapsed() >= Duration::from_millis(20));
        let started = Instant::now();
        assert!(!pause(Duration::from_secs(30), &AtomicBool::new(true)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
