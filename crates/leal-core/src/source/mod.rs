//! Getting a file's bytes without copying them (DESIGN §3.1).
//!
//! [`Source::open`] makes a private snapshot of the user's file. On an
//! internal volume its bytes are one `&[u8]` ([`Source::as_slice`]):
//!
//! 1. **Clone** the file with `fclonefileat(2)` into a temporary folder. On
//!    APFS this is nearly instant and takes no extra disk space
//!    (copy-on-write). `clonefile` only works within one volume, so the
//!    folder must be on the file's own volume: the app gets one from
//!    `FileManager.url(for: .itemReplacementDirectory, …, appropriateFor:)`
//!    and passes it in (ADR-0005 decision 7). If cloning into it fails, or
//!    none is given, the clone goes in Leal's own temporary directory; an
//!    `EXDEV` error there means only that the file is on another volume.
//! 2. **Map** the clone read-only (`memmap2`). Mapping the clone rather than
//!    the original means another program truncating the original can't
//!    crash Leal (SIGBUS), and Leal keeps a stable snapshot of what it
//!    opened.
//!
//! Only when the file's volume can't clone at all (`ENOTSUP`: HFS+, exFAT)
//! and can't vanish (an internal partition) does it fall back: a file of up
//! to [`MEMORY_FALLBACK_MAX_BYTES`] is read into memory, a larger one is
//! copied to Leal's temporary directory and the copy is mapped. Both read
//! the file in full, with ordinary reads, inside `open`. [`Source::storage`]
//! says which happened, so the app can show its status bar note.
//!
//! **Removable drives (ADR-0006).** A file on a removable drive (an
//! external drive, a disk image) is never mapped from that volume, because touching a mapped page of a file whose drive was
//! unplugged kills the process (SIGBUS). Instead it is read with ordinary
//! reads ([`Source::read_range`], which returns an error instead): from a
//! clone on the drive if its volume can clone, or from the user's file
//! itself if it can't. [`Source::stream`] copies it to Leal's temporary
//! directory on the internal disk in chunks, which the index can be built
//! from in the same pass. When the copy is complete, it is mapped and the
//! clone on the drive is deleted. If the drive vanishes first, the source
//! is [`Storage::Disconnected`]: what was copied stays readable, and Save
//! is refused ([`Source::can_save`]). Without a clone, a change to the
//! user's file during the copy is reported ([`Source::changed_on_disk`]).
//! Which volumes can vanish comes from Foundation's "is internal" and "is
//! ejectable" properties, which the app passes in [`VolumeInfo`], and from
//! the volume's own mount flags.
//!
//! **Network shares (ADR-0009)** (SMB, NFS, AFP, WebDAV: a volume without
//! `MNT_LOCAL`, or with a network file system's name) take the same path,
//! with three rules on top. The share is never read on the main thread:
//! [`Source::read_range`] doesn't read bytes of a share that aren't copied
//! yet ([`ReadErrorKind::NotCopied`]), and only first paint
//! ([`Source::read_head`]) and the stream, both in the background, read it.
//! Network errors (`ETIMEDOUT`, `EIO` and the like) are retried for at most
//! [`NETWORK_RETRY_WINDOW`] ([`NETWORK_RETRY_DELAYS`]) before the share
//! counts as [`Storage::Disconnected`], as it does at once after any other
//! failure. `ENOENT` or `ESTALE` from the share is decided by the file's
//! path: nothing there, in a folder still on the share, means another
//! computer deleted it ([`Storage::Deleted`]); another file there means it
//! was replaced ([`ReadErrorKind::ChangedOnDisk`]); the same file, or no
//! folder, is a disconnection.
//!
//! # Reading the bytes
//!
//! - [`Source::as_slice`]: the whole file as one `&[u8]`, for a mapped or
//!   in-memory source. `None` for a file on a removable drive until its
//!   copy is complete.
//! - [`Source::read_range`]: any range: borrowed from the slice when there
//!   is one, read with `pread` otherwise. Rows are read by their extent.
//!   On a network share, only what is copied (every row the index has).
//! - [`Source::read_head`]: the first 64 KB, for first paint. It is
//!   `read_range`, except that on a network share it reads the share, so it
//!   is called off the main thread.
//! - [`Source::stream`]: the whole file once, in order, in chunks of
//!   [`STREAM_CHUNK_BYTES`], cancellable between chunks (ADR-0005 decision
//!   6). This is the pass the index runs on, and for a removable drive it
//!   is also the copy.
//!
//! The clone or copy is deleted when the [`Source`] is dropped. Each
//! temporary folder is recorded first, so [`TempFolders::remove_leftovers`]
//! can remove what a crash left behind, at the next launch.
//!
//! `open` also reads the file's raw extended attributes that later steps
//! need ([`RawAttributes`]), without interpreting them: detection (PLAN 1.2)
//! does that.

mod error;
mod original;
mod removable;
// The one place in leal-core that may use `unsafe` (CLAUDE.md): the system
// calls the standard library doesn't wrap, and the memory map.
#[allow(unsafe_code)]
mod sys;
mod temp;
#[cfg(test)]
pub(crate) mod tests;
mod volume;
mod write;

use std::borrow::Cow;
use std::ffi::CStr;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, BufWriter, Read, Write};
use std::ops::Range;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use memmap2::Mmap;

use error::Step;
pub use error::{OpenError, OpenErrorKind, ReadError, ReadErrorKind};
pub use original::{MOVE_WINDOW, Original, OriginalState, OriginalStatus, PENDING_POLL};
#[cfg(test)]
pub(crate) use removable::PRETEND_MAIN_THREAD;
pub use removable::{NETWORK_RETRY_DELAYS, NETWORK_RETRY_WINDOW, forbid_share_use_on_main_thread};
use removable::{Origin, Removable, ShareRules};
#[cfg(any(test, feature = "test-hooks"))]
pub use removable::{SimulatedFault, SimulatedShare, SimulatedShareFailure};
use temp::TempFolder;
pub use temp::TempFolders;
#[cfg(test)]
use volume::VolumeFlags;
pub(crate) use volume::VolumeKind;
pub use write::Placed;
pub(crate) use write::{
    Existing, Put, Snapshot, Staged, SwapError, as_on_disk, can_write, look_afresh, same_file,
};

/// The largest file the fallback reads into memory, when its volume can't
/// clone: 512 MiB (DESIGN §3.1). A larger file is copied to Leal's
/// temporary directory instead.
pub const MEMORY_FALLBACK_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// The extended attribute macOS apps use to record a file's text encoding,
/// such as `utf-8;134217984` (ADR-0004 decision 11).
pub const TEXT_ENCODING_ATTRIBUTE: &str = "com.apple.TextEncoding";

/// Leal's own extended attribute, which remembers the delimiter and header
/// choice (ADR-0005 decision 1).
pub const INTERPRETATION_ATTRIBUTE: &str = "io.github.robhaswell.leal.interpretation";

/// The size of the chunks [`Source::stream`] delivers: 1 MiB. Each chunk is
/// a few milliseconds of reading even from a slow USB drive, so a cancel
/// takes effect quickly. The index takes each in pieces of its own chunk
/// size ([`crate::index::CHUNK_BYTES`], 256 KiB), so the work between two of
/// its checkpoints stays small.
pub const STREAM_CHUNK_BYTES: usize = 1 << 20;

/// The longest attribute value [`Source::open`] reads. Both attributes are a
/// few dozen bytes; a longer value is treated as absent.
pub const ATTRIBUTE_MAX_BYTES: usize = 64 * 1024;

/// The C-string forms of the attribute names, for `fgetxattr` and, when a
/// save writes them, `fsetxattr`.
pub(crate) const TEXT_ENCODING_ATTRIBUTE_C: &CStr = c"com.apple.TextEncoding";
pub(crate) const INTERPRETATION_ATTRIBUTE_C: &CStr = c"io.github.robhaswell.leal.interpretation";

/// An opened file's bytes: a private, read-only snapshot of the file as it
/// was when it was opened.
///
/// It is `Send` and `Sync`, so a document can share it between threads:
/// every method takes `&self`, including [`stream`](Self::stream), which
/// changes how a file on a removable drive is held.
pub struct Source {
    // Fields are dropped in order: the map goes before the temporary
    // folder that holds the mapped file.
    bytes: Bytes,
    /// Held so that dropping the source deletes the clone or copy
    /// (`TempFolder`'s `Drop`); tests also look inside it. For a file on a
    /// removable drive, this is the internal copy's folder.
    #[cfg_attr(not(test), expect(dead_code, reason = "held for its Drop"))]
    temp: Option<TempFolder>,
    path: PathBuf,
    /// Where the bytes are held, except for a file on a removable drive,
    /// whose storage changes (`Removable::storage`).
    storage: Storage,
    attributes: RawAttributes,
    identity: FileIdentity,
    /// The size of [`stream`](Self::stream)'s chunks:
    /// [`STREAM_CHUNK_BYTES`], except in tests.
    chunk_len: usize,
    /// Where this snapshot's temporary folders went: a save takes the
    /// snapshot of the file it wrote there too (task 2.2).
    folders: TempFolders,
    /// For the snapshot of a file Leal saved ([`Source::from_snapshot`]),
    /// the kind of volume the file is on; `None` otherwise (the bytes say).
    saved_on: Option<VolumeKind>,
}

/// Where a [`Source`]'s bytes are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// A clone of the file, memory-mapped. The normal case.
    Clone,
    /// Read into memory, because the file's volume can't clone. The app
    /// shows a status bar note (DESIGN §3.1).
    Memory,
    /// A copy in Leal's temporary directory, memory-mapped: because the
    /// file's volume can't clone and the file is larger than
    /// [`MEMORY_FALLBACK_MAX_BYTES`], or because the file is on a removable
    /// drive and [`Source::stream`] has finished copying it (ADR-0006).
    Copy,
    /// The file is on a removable drive (ADR-0006). It is read with
    /// ordinary reads, never mapped, from a clone on the drive (or the
    /// user's file, if the drive can't clone), until [`Source::stream`] has
    /// copied it to the internal disk; then it becomes
    /// [`Copy`](Self::Copy).
    Reading,
    /// The file's removable drive was disconnected before its copy was
    /// complete, or its network share stopped answering (ADR-0009). The
    /// first [`Source::available_len`] bytes (what was copied) can still be
    /// read; reads past them fail with [`ReadErrorKind::Disconnected`].
    /// Save is refused, because it needs the bytes that were never read;
    /// Save As is allowed (ADR-0006). The app shows the "drive
    /// disconnected" banner (PLAN 1.7).
    Disconnected,
    /// The file is on a network share and another computer deleted it
    /// before its copy was complete: the share answered `ENOENT` or
    /// `ESTALE`, and nothing is at the file's path, whose folder is still on
    /// the share (ADR-0009). A file replaced by another is a change while
    /// reading instead. As for
    /// [`Disconnected`](Self::Disconnected), the first
    /// [`Source::available_len`] bytes can still be read, Save is refused
    /// and Save As is allowed; reads past them fail with
    /// [`ReadErrorKind::Deleted`]. Unlike a disconnection, it never
    /// reconnects: the file is gone. The app says the file was deleted.
    Deleted,
}

/// What the app knows about the volume a file is on, for
/// [`Source::open_on`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeInfo {
    /// An empty folder on the file's volume, for its clone: see
    /// [`Source::open`].
    pub folder: Option<PathBuf>,
    /// Foundation's `volumeIsInternal` for the file (`URLResourceValues`):
    /// whether the volume is on an internal device. `None` if Foundation
    /// doesn't know (a disk image, for example) or wasn't asked.
    pub is_internal: Option<bool>,
    /// Foundation's `volumeIsEjectable` for the file: whether the volume
    /// can be ejected. `None` if unknown.
    pub is_ejectable: Option<bool>,
}

/// One chunk of [`Source::stream`]: `bytes` are the file's bytes from
/// `offset` on. Chunks arrive in order, with no gaps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// Where the chunk starts in the file.
    pub offset: usize,
    /// The chunk's bytes: [`STREAM_CHUNK_BYTES`] of them, except for the
    /// last chunk.
    pub bytes: &'a [u8],
}

/// The file's extended attributes that later steps need, exactly as stored.
///
/// They are not interpreted here: dialect and encoding detection (PLAN 1.2)
/// decides what they mean and whether to honour them. An attribute is
/// `None` if the file doesn't have it, if it couldn't be read (for example,
/// the volume doesn't support extended attributes), or if it is longer than
/// [`ATTRIBUTE_MAX_BYTES`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawAttributes {
    /// The value of [`TEXT_ENCODING_ATTRIBUTE`] (`com.apple.TextEncoding`).
    pub text_encoding: Option<Vec<u8>>,
    /// The value of [`INTERPRETATION_ATTRIBUTE`]
    /// (`io.github.robhaswell.leal.interpretation`).
    pub interpretation: Option<Vec<u8>>,
}

/// Which file was opened and its state at that moment, from `fstat` on the
/// open file just before it was cloned. Saving compares it with the file on
/// disk to notice changes made elsewhere (DESIGN §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    /// The ID of the volume the file is on (`st_dev`).
    pub device: u64,
    /// The file's inode number on that volume (`st_ino`).
    pub inode: u64,
    /// The file's size in bytes.
    pub len: u64,
    /// When the file's contents last changed, if the volume records it.
    pub modified: Option<SystemTime>,
}

/// The bytes themselves.
enum Bytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
    /// A file on a removable drive (ADR-0006).
    Removable(Box<Removable>),
}

/// How `open` works, so tests can reach paths that normally need large
/// files or a real removable drive.
#[derive(Debug, Clone, Copy)]
struct Options {
    /// The largest file the fallback reads into memory.
    memory_limit: u64,
    volume: VolumeCheck,
    /// The size of [`Source::stream`]'s chunks.
    chunk_len: usize,
    /// A fault to pretend happens to a removable drive (tests only).
    #[cfg(any(test, feature = "test-hooks"))]
    fault: Option<SimulatedFault>,
    /// How a pretend network share behaves (tests only). A pretend share
    /// can't clone, as real ones can't.
    #[cfg(any(test, feature = "test-hooks"))]
    share: Option<SimulatedShare>,
    /// Where a removable drive's copy waits until the test lets it go on
    /// (tests only): `Source::open_holding_copy`.
    #[cfg(any(test, feature = "test-hooks"))]
    hold_at: Option<usize>,
    /// Pretend Leal's temporary folder is on a network volume (tests only).
    #[cfg(test)]
    copy_on_network: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            memory_limit: MEMORY_FALLBACK_MAX_BYTES,
            volume: VolumeCheck::Detect,
            chunk_len: STREAM_CHUNK_BYTES,
            #[cfg(any(test, feature = "test-hooks"))]
            fault: None,
            #[cfg(any(test, feature = "test-hooks"))]
            share: None,
            #[cfg(any(test, feature = "test-hooks"))]
            hold_at: None,
            #[cfg(test)]
            copy_on_network: false,
        }
    }
}

/// Whether to find out if the file's volume can vanish, or (in tests) to
/// assume an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeCheck {
    Detect,
    /// Treat the volume as internal: a disk image standing in for a second
    /// internal disk.
    #[cfg(test)]
    Internal,
    /// Treat the volume as removable: a file in an ordinary temporary
    /// directory standing in for one on a USB drive. Also for benchmarks,
    /// through the `test-hooks` feature (`Source::open_simulating_removable`).
    #[cfg(any(test, feature = "test-hooks"))]
    Removable,
    /// Treat the volume as a network share (ADR-0009):
    /// `Source::open_simulating_share`.
    #[cfg(any(test, feature = "test-hooks"))]
    Network,
}

impl Source {
    /// Opens the file at `path` and takes a snapshot of it.
    ///
    /// `volume_folder` is an empty folder on the same volume as the file,
    /// for its clone: the app passes the one
    /// `FileManager.url(for: .itemReplacementDirectory, …, appropriateFor:)`
    /// made for the file. The source takes it over and deletes it when it is
    /// dropped, or before `open` returns an error. With `None` (the CLI,
    /// tests), the clone goes in `temp`'s scratch directory, which works
    /// for files on that directory's volume; a file elsewhere falls back to
    /// memory or a copy.
    ///
    /// # Errors
    ///
    /// Returns an [`OpenError`] carrying `path`, the OS error code and an
    /// [`OpenErrorKind`]: [`NotFound`](OpenErrorKind::NotFound),
    /// [`PermissionDenied`](OpenErrorKind::PermissionDenied),
    /// [`Directory`](OpenErrorKind::Directory),
    /// [`NotAFile`](OpenErrorKind::NotAFile) (a named pipe, socket or
    /// device), or [`Other`](OpenErrorKind::Other), for example when the
    /// file can't be read or the temporary directory is full.
    ///
    /// ```
    /// use leal_core::source::{Source, Storage, TempFolders};
    /// # let dir = std::env::temp_dir().join(format!("leal-doctest-source-{}", std::process::id()));
    /// # std::fs::create_dir_all(&dir).unwrap();
    /// let path = dir.join("people.csv");
    /// std::fs::write(&path, "name,age\nAda,36\n").unwrap();
    ///
    /// let temp = TempFolders::new(dir.join("scratch"), dir.join("records"));
    /// let source = Source::open(&path, &temp, None).unwrap();
    /// assert_eq!(source.as_slice(), Some(&b"name,age\nAda,36\n"[..]));
    /// assert_eq!(source.storage(), Storage::Clone);
    /// assert_eq!(&*source.read_range(0..8).unwrap(), b"name,age");
    ///
    /// // The snapshot doesn't change when the file does.
    /// std::fs::write(&path, "").unwrap();
    /// assert_eq!(source.as_slice(), Some(&b"name,age\nAda,36\n"[..]));
    /// # drop(source);
    /// # std::fs::remove_dir_all(&dir).unwrap();
    /// ```
    pub fn open(
        path: &Path,
        temp: &TempFolders,
        volume_folder: Option<PathBuf>,
    ) -> Result<Self, OpenError> {
        Self::open_on(
            path,
            temp,
            VolumeInfo {
                folder: volume_folder,
                ..VolumeInfo::default()
            },
        )
    }

    /// [`open`](Self::open), with what the app knows about the file's
    /// volume: the folder for the clone, and Foundation's "is internal" and
    /// "is ejectable" values, which decide (with the volume's mount flags)
    /// whether it is a removable drive (ADR-0006).
    ///
    /// A file on a removable drive opens as [`Storage::Reading`]: nothing
    /// is mapped until [`stream`](Self::stream) has copied it to the
    /// internal disk. A file on the same volume as `temp`'s scratch
    /// directory (the boot volume) is never treated as removable.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    pub fn open_on(path: &Path, temp: &TempFolders, volume: VolumeInfo) -> Result<Self, OpenError> {
        Self::open_with_options(path, temp, volume, Options::default())
    }

    /// TEST HOOK, not for product code: opens `path` as if it were on a
    /// removable drive (ADR-0006), so the removable path (ordinary reads,
    /// then [`stream`](Self::stream) copying it in chunks of `chunk_len`)
    /// can be measured and tested without a real drive. Only built for
    /// leal-core's tests and with the `test-hooks` feature, which the
    /// benchmarks and the app's tests turn on, and the shipped app never
    /// does.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn open_simulating_removable(
        path: &Path,
        temp: &TempFolders,
        chunk_len: usize,
    ) -> Result<Self, OpenError> {
        Self::open_with_options(
            path,
            temp,
            VolumeInfo::default(),
            Options {
                volume: VolumeCheck::Removable,
                chunk_len,
                ..Options::default()
            },
        )
    }

    /// TEST HOOK, not for product code: as
    /// [`open_simulating_removable`](Self::open_simulating_removable), and
    /// `fault` happens when the copy reaches its offset: the drive vanishes
    /// ([`Storage::Disconnected`]) or the file changes
    /// ([`changed_on_disk`](Self::changed_on_disk)). The app's tests use it
    /// (through leal-ffi's `test-exports`) for the banners of task 1.7.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn open_simulating_fault(
        path: &Path,
        temp: &TempFolders,
        chunk_len: usize,
        fault: Option<SimulatedFault>,
    ) -> Result<Self, OpenError> {
        Self::open_with_options(
            path,
            temp,
            VolumeInfo::default(),
            Options {
                volume: VolumeCheck::Removable,
                chunk_len,
                fault,
                ..Options::default()
            },
        )
    }

    /// TEST HOOK, not for product code: opens `path` as if it were on a
    /// network share (ADR-0009) that can't clone and behaves as `share`
    /// says: slow, or failing with a given error. The source reads the
    /// user's file itself, copies it in chunks of `chunk_len`, and follows
    /// a share's rules: [`read_range`](Self::read_range) never reads what
    /// isn't copied, network errors are retried after `share`'s delays, and
    /// `ENOENT` or `ESTALE` is decided by the path. For the tests of
    /// task 2.0, through leal-ffi's `test-exports` too.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn open_simulating_share(
        path: &Path,
        temp: &TempFolders,
        chunk_len: usize,
        share: SimulatedShare,
    ) -> Result<Self, OpenError> {
        Self::open_with_options(
            path,
            temp,
            VolumeInfo::default(),
            Options {
                volume: VolumeCheck::Network,
                chunk_len,
                share: Some(share),
                ..Options::default()
            },
        )
    }

    /// TEST HOOK, not for product code: [`open_on`](Self::open_on), the
    /// real thing (the volume is looked at as for any file), except that if
    /// the file is on a removable drive or a network share, the copy's
    /// reads that reach past byte `hold_at` wait until
    /// [`release_held_copy`](Self::release_held_copy), or until the pass is
    /// cancelled (closing the document cancels it). So a test can pull a
    /// real drive while the copy is certainly part-way through, however
    /// long the pull takes, and only then let the copy go on and find the
    /// drive gone. First paint's read isn't held. The app's tests use it
    /// (through leal-ffi's `test-exports`) with real disk images.
    ///
    /// # Errors
    ///
    /// As for [`open`](Self::open).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn open_holding_copy(
        path: &Path,
        temp: &TempFolders,
        volume: VolumeInfo,
        hold_at: usize,
    ) -> Result<Self, OpenError> {
        Self::open_with_options(
            path,
            temp,
            volume,
            Options {
                hold_at: Some(hold_at),
                ..Options::default()
            },
        )
    }

    /// [`open_on`](Self::open_on), with options so tests can reach the copy
    /// fallback with small files, choose the stream's chunk size, and
    /// pretend a volume is internal, removable or a network share.
    fn open_with_options(
        path: &Path,
        temp: &TempFolders,
        mut volume: VolumeInfo,
        options: Options,
    ) -> Result<Self, OpenError> {
        // Take over the given folder first, so it is removed on every early
        // return below.
        let volume_folder = volume.folder.take().map(GivenFolder);
        let (file, identity) = open_regular(path)?;
        let attributes = RawAttributes::read(&file);

        let kind = volume_kind(&file, &identity, temp, &volume, options.volume);
        // The one routing decision for ADR-0006 option C: removable drives
        // and network shares (ADR-0009) are both read with ordinary reads
        // and copied in the background, never mapped from their volume.
        let removable = kind != VolumeKind::Fixed;
        let removable_source = |origin| -> Result<_, OpenError> {
            let copy_error = |error| OpenError::new(path, Step::Copy, error);
            let copy = temp.create().map_err(copy_error)?;
            let share = (kind == VolumeKind::Network).then(|| share_rules(&options));
            #[cfg_attr(
                not(any(test, feature = "test-hooks")),
                expect(unused_mut, reason = "only the test hook changes it")
            )]
            let mut removable = Removable::new(path, origin, &copy, options.chunk_len, share)?;
            #[cfg(any(test, feature = "test-hooks"))]
            {
                removable.set_fault(options.fault);
                let share_hold = options.share.and_then(|share| share.hold_at);
                removable.hold_copy_at(options.hold_at.or(share_hold));
            }
            #[cfg(test)]
            if options.copy_on_network {
                removable.set_copy_on_network();
            }
            if kind == VolumeKind::Network {
                // Opening a file on a share used the share: never on the
                // main thread (ADR-0009). Debug builds panic here if so.
                removable.note_share_use();
            }
            Ok((
                Bytes::Removable(Box::new(removable)),
                Some(copy),
                Storage::Reading,
            ))
        };
        // A share isn't cloned, or even asked to: nothing is made on the
        // user's share (task 2.0 review), and smbfs and nfs can't clone
        // anyway. It is read from the user's file itself. A folder the app
        // gave for it is removed as it is dropped (the app gives none for a
        // share).
        let cloned = if kind == VolumeKind::Network {
            drop(volume_folder);
            None
        } else {
            clone(&file, path, temp, volume_folder)?
        };
        let (bytes, temp_folder, storage) = match cloned {
            Some(clone) if removable => removable_source(Origin::Clone(clone))?,
            Some(clone) => {
                let map = map_file(path, &clone.file_path())?;
                (Bytes::Mapped(map), Some(clone), Storage::Clone)
            }
            // A removable drive that can't clone (exFAT, FAT, HFS+), or a
            // network share: read the user's file itself, never mapped, and
            // stream it to the internal disk, rather than reading it all
            // before first paint.
            None if removable => removable_source(Origin::Original {
                file,
                path: path.to_owned(),
                identity,
            })?,
            None if identity.len <= options.memory_limit => {
                let bytes = read_into_memory(&file, path, identity.len)?;
                (Bytes::Owned(bytes), None, Storage::Memory)
            }
            None => {
                let copy = copy_to_scratch(&file, path, temp, identity.len)?;
                let map = map_file(path, &copy.file_path())?;
                (Bytes::Mapped(map), Some(copy), Storage::Copy)
            }
        };

        Ok(Self {
            bytes,
            temp: temp_folder,
            path: path.to_owned(),
            storage,
            attributes,
            identity,
            chunk_len: options.chunk_len.max(1),
            folders: temp.clone(),
            saved_on: None,
        })
    }

    /// The whole file as one slice, exactly as it was on disk when it was
    /// opened, if there is one: always for a file on an internal volume
    /// (mapped, or in memory), and for a file on a removable drive once
    /// [`stream`](Self::stream) has copied it to the internal disk and
    /// mapped the copy. `None` until then ([`Storage::Reading`] and
    /// [`Storage::Disconnected`]); use [`read_range`](Self::read_range),
    /// which works in every case.
    #[must_use]
    pub fn as_slice(&self) -> Option<&[u8]> {
        match &self.bytes {
            Bytes::Mapped(map) => Some(map),
            Bytes::Owned(bytes) => Some(bytes),
            Bytes::Removable(removable) => removable.as_slice(),
        }
    }

    /// The bytes in `range`, as they were on disk when the file was opened.
    /// Like `pread`, a range that runs past the end of the file is cut
    /// short there (and one that starts past it gives no bytes).
    ///
    /// The bytes are borrowed when there is a slice to borrow from
    /// ([`as_slice`](Self::as_slice)), which costs nothing. For a file on a
    /// removable drive whose copy isn't complete, they are read with
    /// `pread` into a new buffer: from the internal copy if it has them,
    /// otherwise from the drive (the clone there, or the user's file itself
    /// if the drive can't clone). Without a clone there is no snapshot, so
    /// each such read checks the file's size and modification time
    /// afterwards; bytes read after a detected change are never returned.
    /// This never waits: it may be called on the main thread (DESIGN §3.9).
    /// So on a network share it never reads the share itself (ADR-0009):
    /// only bytes already in the internal copy can be read, which every row
    /// the index has is.
    ///
    /// # Errors
    ///
    /// Only for a file on a removable drive or a network share before its
    /// copy is complete: [`ReadErrorKind::Disconnected`] if the drive has
    /// vanished (or the share stopped answering) and the range wasn't
    /// copied before it did (the source is then [`Storage::Disconnected`]),
    /// [`ReadErrorKind::Deleted`] if the file on a share was deleted
    /// elsewhere before the range was copied, [`ReadErrorKind::NotCopied`]
    /// for a range of a share that isn't copied yet,
    /// [`ReadErrorKind::ChangedOnDisk`] if the user's file (read without a
    /// clone) has changed since it was opened, or [`ReadErrorKind::Other`]
    /// for any other read error, which leaves the source as it was.
    pub fn read_range(&self, range: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
        let range = clamp(range, self.len_usize());
        match &self.bytes {
            Bytes::Mapped(map) => Ok(Cow::Borrowed(&map[range])),
            Bytes::Owned(bytes) => Ok(Cow::Borrowed(&bytes[range])),
            Bytes::Removable(removable) => removable.read_range(range),
        }
    }

    /// The first `len` bytes of the file (all of it, if it is shorter), for
    /// first paint: [`read_range`](Self::read_range)`(0..len)`, except on a
    /// network share, where it reads the share itself (ADR-0009). That read
    /// may block for as long as the share takes to answer, and a network
    /// error is retried for up to [`NETWORK_RETRY_WINDOW`], so **on a share
    /// it must be called off the main thread**. A debug build panics if it
    /// isn't, in an app ([`forbid_share_use_on_main_thread`]). Nothing can
    /// cancel it: no open can be cancelled. On a removable drive or a
    /// share, the bytes are kept until the copy's first chunks are checked
    /// against them (`ChangedOnDisk` if they differ). That matters when the
    /// user's file itself is read: on a share (which is never cloned), on a
    /// drive that can't clone, and on one that comes back without its
    /// clone.
    ///
    /// # Errors
    ///
    /// As for [`read_range`](Self::read_range), except
    /// [`ReadErrorKind::NotCopied`].
    pub fn read_head(&self, len: usize) -> Result<Cow<'_, [u8]>, ReadError> {
        match &self.bytes {
            Bytes::Removable(removable) => removable.read_head(len),
            Bytes::Mapped(_) | Bytes::Owned(_) => self.read_range(0..len),
        }
    }

    /// Reads the whole file once, from the start, calling `on_chunk` with
    /// each [`Chunk`] in order. This is the pass the row index is built from
    /// (task 1.3a joins the two).
    ///
    /// `cancel` is checked before each chunk, so the pass stops within one
    /// chunk of it being set (ADR-0005 decision 6).
    ///
    /// For a file on an internal volume the chunks are borrowed from the
    /// map or memory, and nothing else happens. For a file on a removable
    /// drive (ADR-0006) or a network share (ADR-0009), each chunk is also
    /// written to Leal's copy on the
    /// internal disk before it is handed over; after the last one, the copy
    /// is mapped (the source becomes [`Storage::Copy`]) and the clone on
    /// the drive, if there is one, is deleted. Without a clone (a drive that
    /// can't clone), the user's file is checked for changes after every
    /// chunk is read, and a chunk read after a change is never delivered. A
    /// pass that is cancelled or fails can be run again: it starts from the
    /// beginning, reading what was already copied from the internal copy.
    /// Only one pass runs at a time; a second call waits for the first to
    /// finish (so `on_chunk` must not call `stream` itself). `on_chunk` may
    /// call [`read_range`](Self::read_range). Run it on a background thread:
    /// it reads the whole file, after an `EIO` it waits briefly to tell a
    /// pulled drive from a bad block, and on a share it retries a network
    /// error for up to about 3 s (a cancel stops the wait).
    ///
    /// # Errors
    ///
    /// [`ReadErrorKind::Cancelled`] if `cancel` was set. For a file on a
    /// removable drive, also:
    /// - [`ReadErrorKind::Disconnected`] when the pass reaches bytes that
    ///   weren't copied before the drive vanished, or the share stopped
    ///   answering (the chunks before them have been delivered);
    /// - [`ReadErrorKind::Deleted`] likewise, when the file on a share was
    ///   deleted elsewhere;
    /// - [`ReadErrorKind::ChangedOnDisk`] when the user's file (read
    ///   without a clone) has changed since it was opened, including being
    ///   truncated or appended to. The pass stops, the copy is never mapped,
    ///   and every later pass returns this straight away
    ///   ([`changed_on_disk`](Self::changed_on_disk));
    /// - [`ReadErrorKind::Other`] for another read error or a failed write
    ///   to the internal disk (for example, because it is full).
    pub fn stream(
        &self,
        cancel: &AtomicBool,
        on_chunk: impl FnMut(Chunk<'_>),
    ) -> Result<(), ReadError> {
        match &self.bytes {
            Bytes::Mapped(map) => stream_slice(map, self.chunk_len, cancel, on_chunk),
            Bytes::Owned(bytes) => stream_slice(bytes, self.chunk_len, cancel, on_chunk),
            Bytes::Removable(removable) => removable.stream(cancel, on_chunk),
        }
    }

    /// The snapshot's size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        // A length always fits in u64 on Apple's 64-bit platforms.
        u64::try_from(self.len_usize()).unwrap_or(u64::MAX)
    }

    /// Whether the file was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len_usize() == 0
    }

    /// How many bytes, from the start of the file, can be read: all of
    /// them, except after the file's removable drive was disconnected
    /// ([`Storage::Disconnected`]) or the file on its share was deleted
    /// ([`Storage::Deleted`]), when it is what had been copied.
    #[must_use]
    pub fn available_len(&self) -> u64 {
        let available = match &self.bytes {
            Bytes::Removable(removable) => removable.available_len(),
            Bytes::Mapped(_) | Bytes::Owned(_) => self.len_usize(),
        };
        u64::try_from(available).unwrap_or(u64::MAX)
    }

    /// Whether Save (writing over the original) is possible. `false`:
    /// - once the file's removable drive was disconnected before its copy
    ///   was complete ([`Storage::Disconnected`]), because Save needs the
    ///   bytes that were never read (ADR-0006). Save As is allowed; it
    ///   writes the [`available_len`](Self::available_len) bytes, and the
    ///   app explains that the rest is missing;
    /// - likewise once the file on a network share was deleted elsewhere
    ///   before its copy was complete ([`Storage::Deleted`]);
    /// - once the user's file changed while it was being read
    ///   ([`changed_on_disk`](Self::changed_on_disk)), because the bytes
    ///   Leal holds may mix old and new contents, and Save would write that
    ///   mix over the file. The app offers Reload (task 1.9).
    #[must_use]
    pub fn can_save(&self) -> bool {
        !matches!(self.storage(), Storage::Disconnected | Storage::Deleted)
            && !self.changed_on_disk()
    }

    /// Whether the file is on a network share (ADR-0009): it is read like a
    /// file on a removable drive, and the share is never read on the main
    /// thread. It stays `true` once the copy is complete. The app opens and
    /// reloads such a file off the main thread.
    #[must_use]
    pub fn is_on_network_share(&self) -> bool {
        match self.saved_on {
            Some(kind) => kind == VolumeKind::Network,
            None => self.is_share_bytes(),
        }
    }

    /// Whether the file is on a volume that can vanish (a removable drive
    /// or a network share), so it was never mapped from there.
    #[must_use]
    pub fn can_vanish(&self) -> bool {
        match self.saved_on {
            Some(kind) => kind != VolumeKind::Fixed,
            None => matches!(self.bytes, Bytes::Removable(_)),
        }
    }

    /// The snapshot of the file Leal just saved at `path` (task 2.2), made
    /// as it was written ([`Staged::snapshot`]): a clone, or a copy on the
    /// internal disk, mapped. It is never read from a volume that can
    /// vanish, and isn't read again at all: the save knows its `identity`,
    /// the `attributes` it wrote and the `kind` of volume it is on.
    ///
    /// # Errors
    ///
    /// If the snapshot can't be mapped.
    pub(crate) fn from_snapshot(
        path: &Path,
        snapshot: Snapshot,
        identity: FileIdentity,
        attributes: RawAttributes,
        temps: &TempFolders,
        kind: VolumeKind,
    ) -> Result<Source, OpenError> {
        let map = map_file(path, &snapshot.folder.file_path())?;
        Ok(Source {
            bytes: Bytes::Mapped(map),
            temp: Some(snapshot.folder),
            path: path.to_owned(),
            storage: snapshot.storage,
            attributes,
            identity,
            chunk_len: STREAM_CHUNK_BYTES,
            folders: temps.clone(),
            saved_on: Some(kind),
        })
    }

    fn is_share_bytes(&self) -> bool {
        match &self.bytes {
            Bytes::Removable(removable) => removable.is_share(),
            Bytes::Mapped(_) | Bytes::Owned(_) => false,
        }
    }

    /// TEST HOOK, not for product code: how many times the file's network
    /// share was read on the main thread, which ADR-0009 forbids (a debug
    /// build also panics). The app's tests check that it stays 0.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn share_reads_on_main_thread(&self) -> usize {
        match &self.bytes {
            Bytes::Removable(removable) => removable.share_reads_on_main_thread(),
            Bytes::Mapped(_) | Bytes::Owned(_) => 0,
        }
    }

    /// Where the user's file is now, as the watcher follows it (task 1.9):
    /// a look at a file read without a clone (`ENOENT` or `ESTALE` from a
    /// share) uses this path, not the one it was opened at.
    pub(crate) fn note_original_path(&self, path: &Path) {
        if let Bytes::Removable(removable) = &self.bytes {
            removable.note_original_path(path);
        }
    }

    /// For tests of a simulated share: the callback its files' closing
    /// threads call. `None` otherwise, and always in the shipped app.
    #[cfg_attr(
        not(any(test, feature = "test-hooks")),
        expect(clippy::unused_self, reason = "only the test hooks have a callback")
    )]
    pub(crate) fn share_close_hook(&self) -> Option<fn()> {
        #[cfg(any(test, feature = "test-hooks"))]
        if let Bytes::Removable(removable) = &self.bytes {
            return removable.share_close_hook();
        }
        None
    }

    /// The file's share is about to be used outside this source (the
    /// watcher's look at the file, `Document::check_original`): never on the
    /// main thread (ADR-0009). Nothing for a file that isn't on a share.
    pub(crate) fn note_share_use(&self) {
        if let Bytes::Removable(removable) = &self.bytes
            && removable.is_share()
        {
            removable.note_share_use();
        }
    }

    /// TEST HOOK, not for product code: for a source from
    /// [`open_simulating_share`](Self::open_simulating_share) with a
    /// `hold_at`, the held reads go on. The same as
    /// [`release_held_copy`](Self::release_held_copy).
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn simulated_share_release(&self) {
        self.release_held_copy();
    }

    /// TEST HOOK, not for product code: for a source from
    /// [`open_holding_copy`](Self::open_holding_copy) (or a simulated share
    /// with a `hold_at`), the copy's held reads go on, and no later one is
    /// held. Does nothing for any other source.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn release_held_copy(&self) {
        if let Bytes::Removable(removable) = &self.bytes {
            removable.release_held_copy();
        }
    }

    /// TEST HOOK, not for product code: how many first paint reads of a
    /// simulated share there have been.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn simulated_head_reads(&self) -> usize {
        match &self.bytes {
            Bytes::Removable(removable) => removable.simulated_head_reads(),
            Bytes::Mapped(_) | Bytes::Owned(_) => 0,
        }
    }

    /// TEST HOOK, not for product code: for a source from
    /// [`open_simulating_share`](Self::open_simulating_share), how many
    /// reads of the share have been tried (retries included), and how many
    /// of them were made to fail.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn simulated_share_reads(&self) -> (usize, usize) {
        match &self.bytes {
            Bytes::Removable(removable) => removable.simulated_share_reads(),
            Bytes::Mapped(_) | Bytes::Owned(_) => (0, 0),
        }
    }

    /// Whether the user's file changed on disk while Leal was reading it.
    /// Only a file on a removable drive that can't clone is read without a
    /// snapshot, so only then can this be `true`. It is checked after every
    /// read of the file: each chunk [`stream`](Self::stream) reads, and
    /// each [`read_range`](Self::read_range) that isn't served from the
    /// copy. Once it is `true`, it stays `true`; the copy is never mapped,
    /// reads that would need the file give
    /// [`ReadErrorKind::ChangedOnDisk`], and [`can_save`](Self::can_save)
    /// is `false`.
    ///
    /// **Rows read before the change was detected must be discarded**, not
    /// only the ones after it: first paint may have read the old bytes, and
    /// a change between two checks can't be placed exactly, so the rows the
    /// app holds may come from two versions of the file. The app drops its
    /// row cache and offers Reload (task 1.9). A change after the copy is
    /// complete doesn't affect the copy, and is the file watcher's to
    /// notice.
    #[must_use]
    pub fn changed_on_disk(&self) -> bool {
        match &self.bytes {
            Bytes::Removable(removable) => removable.changed_on_disk(),
            Bytes::Mapped(_) | Bytes::Owned(_) => false,
        }
    }

    /// The watcher saw the user's file written to ([`Original`], task
    /// 1.9). For a file on a removable drive that can't clone, which is
    /// read from the user's file itself, that is a change while reading
    /// ([`changed_on_disk`](Self::changed_on_disk) becomes `true`): the
    /// kernel's write event catches a same-size write that the size and
    /// modification time checks can miss. Everything else holds a snapshot
    /// (a clone, a complete copy, or memory), which a write can't reach, so
    /// for them this does nothing.
    pub fn note_original_written(&self) {
        if let Bytes::Removable(removable) = &self.bytes {
            removable.note_original_written();
        }
    }

    /// The file's removable drive is back (task 1.9; the app notices a
    /// volume mounting). If the source was [`Storage::Disconnected`] before
    /// its copy was complete, it reopens the clone on the drive, or failing
    /// that the user's file at `original` (where it is now) if its inode,
    /// size and modification time are what they were when it was opened.
    /// The source is then [`Storage::Reading`] again, Save is allowed again,
    /// and the next [`stream`](Self::stream) carries on copying from
    /// [`available_len`](Self::available_len). Returns whether it
    /// reconnected; `false` for every other source, and if the drive's file
    /// can't be found or has changed.
    pub fn reconnect(&self, original: &Path) -> bool {
        match &self.bytes {
            Bytes::Removable(removable) => removable.reconnect(original, &self.identity),
            Bytes::Mapped(_) | Bytes::Owned(_) => false,
        }
    }

    /// TEST HOOK, not for product code: the drive of a source opened with
    /// [`open_simulating_fault`](Self::open_simulating_fault) is plugged
    /// back in, or the share of one opened with
    /// [`open_simulating_share`](Self::open_simulating_share) answers
    /// again. A simulated drive stays away until then, so the app's
    /// checks (on activation, on a volume mounting) can't bring it back by
    /// themselves. [`reconnect`](Self::reconnect) then works as for a real
    /// drive.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub fn simulate_drive_back(&self) {
        if let Bytes::Removable(removable) = &self.bytes {
            removable.simulate_drive_back();
        }
    }

    /// TEST HOOK, not for product code: Leal's clone of a file on a
    /// removable drive is lost (deleted) while the drive is away, so
    /// [`reconnect`](Self::reconnect) falls back to the user's file (task
    /// 2.1a). Returns whether there was a clone to lose: `false` for a
    /// drive that can't clone, a network share, a complete copy, and every
    /// other source.
    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    #[must_use]
    pub fn simulate_clone_lost(&self) -> bool {
        match &self.bytes {
            Bytes::Removable(removable) => removable.simulate_clone_lost(),
            Bytes::Mapped(_) | Bytes::Owned(_) => false,
        }
    }

    fn len_usize(&self) -> usize {
        match &self.bytes {
            Bytes::Mapped(map) => map.len(),
            Bytes::Owned(bytes) => bytes.len(),
            Bytes::Removable(removable) => removable.len(),
        }
    }

    /// The path the file was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the bytes are held: a clone, memory or a copy, or, for a file
    /// on a removable drive, read from the drive or disconnected from it.
    /// That last one changes as [`stream`](Self::stream) runs, or when the
    /// drive vanishes.
    #[must_use]
    pub fn storage(&self) -> Storage {
        match &self.bytes {
            Bytes::Removable(removable) => removable.storage(),
            Bytes::Mapped(_) | Bytes::Owned(_) => self.storage,
        }
    }

    /// The file's raw extended attributes, for detection.
    #[must_use]
    pub fn attributes(&self) -> &RawAttributes {
        &self.attributes
    }

    /// Which file was opened, and its size and modification time then.
    #[must_use]
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }

    /// The temporary folders the snapshot was made in.
    #[must_use]
    pub fn temp_folders(&self) -> &TempFolders {
        &self.folders
    }

    /// The clone on the removable drive, while there is one.
    #[cfg(test)]
    fn external_clone(&self) -> Option<PathBuf> {
        match &self.bytes {
            Bytes::Removable(removable) => removable.external_clone(),
            Bytes::Mapped(_) | Bytes::Owned(_) => None,
        }
    }

    /// How many of first paint's bytes a file on a removable drive keeps
    /// for its copy to check.
    #[cfg(test)]
    fn kept_head_len(&self) -> usize {
        match &self.bytes {
            Bytes::Removable(removable) => removable.kept_head_len(),
            Bytes::Mapped(_) | Bytes::Owned(_) => 0,
        }
    }

    /// What an `EIO` from a removable drive's file would be classed as now
    /// (`None` for other sources).
    #[cfg(test)]
    fn classify_eio_now(&self) -> Option<ReadErrorKind> {
        match &self.bytes {
            Bytes::Removable(removable) => Some(removable.classify_eio_now().kind()),
            Bytes::Mapped(_) | Bytes::Owned(_) => None,
        }
    }

    /// Lets go of the clone on the removable drive, if any, as a crash
    /// would.
    #[cfg(test)]
    fn abandon_external_clone(&mut self) {
        if let Bytes::Removable(removable) = &self.bytes {
            removable.abandon_external_clone();
        }
    }
}

/// `range`, cut short at `len`.
fn clamp(range: Range<usize>, len: usize) -> Range<usize> {
    let end = range.end.min(len);
    range.start.min(end)..end
}

/// [`Source::stream`] over bytes that are all in hand.
fn stream_slice(
    bytes: &[u8],
    chunk_len: usize,
    cancel: &AtomicBool,
    mut on_chunk: impl FnMut(Chunk<'_>),
) -> Result<(), ReadError> {
    let mut offset = 0;
    for chunk in bytes.chunks(chunk_len.max(1)) {
        if cancel.load(Ordering::Relaxed) {
            return Err(ReadError::cancelled());
        }
        on_chunk(Chunk {
            offset,
            bytes: chunk,
        });
        offset += chunk.len();
    }
    Ok(())
}

/// The rules a share is read with: a real share's, or (tests only) a
/// pretend one's.
fn share_rules(options: &Options) -> ShareRules {
    #[cfg(any(test, feature = "test-hooks"))]
    if let Some(share) = &options.share {
        return ShareRules::simulated(*share);
    }
    #[cfg(not(any(test, feature = "test-hooks")))]
    let _ = options;
    ShareRules::new()
}

/// What kind of volume `folder` is on (a save's destination's folder),
/// as for a file there ([`volume_kind`]), with what the app knows about it.
///
/// # Errors
///
/// If the folder can't be opened.
pub(crate) fn kind_of_folder(
    folder: &Path,
    temp: &TempFolders,
    volume: &VolumeInfo,
) -> io::Result<VolumeKind> {
    let file = File::open(folder)?;
    let metadata = file.metadata()?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    Ok(volume_kind(
        &file,
        &identity,
        temp,
        volume,
        VolumeCheck::Detect,
    ))
}

/// What kind of volume the file is on, for how to read it: a fixed volume
/// is mapped; a removable drive (ADR-0006) or a network share (ADR-0009)
/// is read with ordinary reads and copied in the background.
fn volume_kind(
    file: &File,
    identity: &FileIdentity,
    temp: &TempFolders,
    volume: &VolumeInfo,
    check: VolumeCheck,
) -> VolumeKind {
    match check {
        VolumeCheck::Detect => {}
        #[cfg(test)]
        VolumeCheck::Internal => return VolumeKind::Fixed,
        #[cfg(any(test, feature = "test-hooks"))]
        VolumeCheck::Removable => return VolumeKind::Removable,
        #[cfg(any(test, feature = "test-hooks"))]
        VolumeCheck::Network => return VolumeKind::Network,
    }
    // A share first, before the shortcut below: a network home folder puts
    // the scratch directory on the share too, and its files are still on a
    // share (task 2.0 review). One `fstatfs`, about a microsecond.
    let flags = volume::flags(file).ok();
    if flags.is_some_and(|flags| !flags.local || flags.network_type) {
        return VolumeKind::Network;
    }
    // The scratch directory is on the boot volume, which can't vanish
    // without the Mac going with it, and copying a file to its own volume
    // would gain nothing. This also keeps opening a file there fast: one
    // `stat`. The scratch directory may not exist yet, so this asks its
    // nearest folder that does.
    let on_scratch_volume = temp
        .scratch()
        .ancestors()
        .find_map(|folder| fs::metadata(folder).ok())
        .is_some_and(|scratch| scratch.dev() == identity.device);
    if on_scratch_volume {
        return VolumeKind::Fixed;
    }
    volume::kind(volume, flags)
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the bytes themselves, which may be gigabytes.
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("len", &self.len())
            .field("storage", &self.storage())
            .field("attributes", &self.attributes)
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl RawAttributes {
    fn read(file: &File) -> Self {
        let read = |name| {
            sys::read_xattr(file, name, ATTRIBUTE_MAX_BYTES)
                .ok()
                .flatten()
        };
        Self {
            text_encoding: read(TEXT_ENCODING_ATTRIBUTE_C),
            interpretation: read(INTERPRETATION_ATTRIBUTE_C),
        }
    }
}

/// An empty folder the app made for a clone. It is removed if `open` fails
/// before the folder is recorded and taken over by a [`TempFolder`].
struct GivenFolder(PathBuf);

impl GivenFolder {
    /// Forgets the folder without removing it, once a [`TempFolder`] owns
    /// it.
    fn release(self) {
        let mut this = std::mem::ManuallyDrop::new(self);
        drop(std::mem::take(&mut this.0));
    }
}

impl Drop for GivenFolder {
    fn drop(&mut self) {
        // Only removes it if it is still empty.
        let _ = fs::remove_dir(&self.0);
    }
}

/// Opens `path` for reading and checks that it is a regular file.
///
/// It opens with `O_NONBLOCK`, so a named pipe can't make it wait forever
/// for a writer, and clears it again once the file is known to be regular.
/// The check uses `fstat` on the open file, so it is about the file that
/// was opened, even if the path changes meanwhile.
pub(crate) fn open_regular(path: &Path) -> Result<(File, FileIdentity), OpenError> {
    let open_error = |error| OpenError::new(path, Step::Open, error);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        // A socket can't be opened at all (`EOPNOTSUPP`).
        Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {
            let is_socket =
                fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket());
            return Err(if is_socket {
                OpenError::not_a_file(path)
            } else {
                open_error(error)
            });
        }
        Err(error) => return Err(open_error(error)),
    };
    let metadata = file.metadata().map_err(open_error)?;
    if metadata.is_dir() {
        return Err(OpenError::directory(path));
    }
    if !metadata.is_file() {
        return Err(OpenError::not_a_file(path));
    }
    sys::clear_nonblocking(&file).map_err(open_error)?;
    let identity = FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    Ok((file, identity))
}

/// Clones `file` into a temporary folder: `volume_folder` if given, else
/// the scratch directory. Returns `None` if the volume can't clone, so the
/// caller falls back to memory or a copy.
fn clone(
    file: &File,
    path: &Path,
    temp: &TempFolders,
    volume_folder: Option<GivenFolder>,
) -> Result<Option<TempFolder>, OpenError> {
    let clone_error = |error| OpenError::new(path, Step::Clone, error);

    if let Some(given) = volume_folder {
        // If recording fails, `given` is dropped and removes the folder.
        let folder = temp.adopt(given.0.clone()).map_err(clone_error)?;
        given.release();
        match clone_into(file, &folder) {
            Ok(()) => return Ok(Some(folder)),
            // The file's own volume can't clone.
            Err(error) if error.raw_os_error() == Some(libc::ENOTSUP) => return Ok(None),
            // Anything else (it isn't on the file's volume after all, or
            // isn't writable): clone elsewhere. `folder` is removed here.
            Err(_) => {}
        }
    }

    let folder = temp.create().map_err(clone_error)?;
    match clone_into(file, &folder) {
        Ok(()) => Ok(Some(folder)),
        // EXDEV: the file is on another volume, where no folder was given
        // (or the given one failed). ENOTSUP: this volume can't clone.
        Err(error) if matches!(error.raw_os_error(), Some(libc::EXDEV | libc::ENOTSUP)) => Ok(None),
        Err(error) => Err(clone_error(error)),
    }
}

/// Clones `file` into `folder`, then makes the clone read-only on disk.
fn clone_into(file: &File, folder: &TempFolder) -> io::Result<()> {
    let clone = folder.file_path();
    sys::clone_file(file, &clone)?;
    // The clone gets the original's BSD flags. A Finder-locked (`uchg`) or
    // append-only (`uappnd`) original would give a clone whose mode can't be
    // changed and which can't be deleted, so clear those first.
    temp::unlock(&clone)?;
    // The clone also gets the original's permissions. Make it read-only for
    // everyone, so nothing writes to it while it is mapped (`map_read_only`
    // relies on this).
    fs::set_permissions(&clone, Permissions::from_mode(0o400))
}

/// Maps the clone or copy at `path`, read-only. `original` is the user's
/// file, for the error.
fn map_file(original: &Path, path: &Path) -> Result<Mmap, OpenError> {
    let map_error = |error| OpenError::new(original, Step::Map, error);
    let file = File::open(path).map_err(map_error)?;
    sys::map_read_only(&file).map_err(map_error)
}

/// Reads the first `len` bytes of `file` (its size when opened) into
/// memory.
fn read_into_memory(file: &File, path: &Path, len: u64) -> Result<Vec<u8>, OpenError> {
    let read_error = |error| OpenError::new(path, Step::Read, error);
    let capacity = usize::try_from(len)
        .map_err(|_| read_error(io::Error::from(io::ErrorKind::OutOfMemory)))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| read_error(io::Error::from(io::ErrorKind::OutOfMemory)))?;
    // `take` stops at the size the file had when opened, so a file that is
    // growing gives a consistent snapshot rather than an unbounded read.
    file.take(len).read_to_end(&mut bytes).map_err(read_error)?;
    Ok(bytes)
}

/// Copies the first `len` bytes of `file` into a new folder in the scratch
/// directory.
fn copy_to_scratch(
    file: &File,
    path: &Path,
    temp: &TempFolders,
    len: u64,
) -> Result<TempFolder, OpenError> {
    let copy_error = |error| OpenError::new(path, Step::Copy, error);
    let folder = temp.create().map_err(copy_error)?;
    let copy_path = folder.file_path();
    let copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&copy_path)
        .map_err(copy_error)?;
    // A large buffer: `io::copy` reads straight into a `BufWriter`'s buffer.
    let mut writer = BufWriter::with_capacity(1024 * 1024, copy);
    io::copy(&mut file.take(len), &mut writer).map_err(copy_error)?;
    writer.flush().map_err(copy_error)?;
    fs::set_permissions(&copy_path, Permissions::from_mode(0o400)).map_err(copy_error)?;
    Ok(folder)
}
