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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use memmap2::Mmap;

use super::error::Step;
use super::temp::TempFolder;
use super::{Chunk, FileIdentity, OpenError, ReadError, Storage, sys};

/// How long to wait before asking again whether the drive has gone, after
/// an `EIO` that doesn't look like a disconnection yet. When a drive is
/// pulled, reads can fail a moment before the kernel unmounts the volume.
const EIO_RECHECK_DELAY: Duration = Duration::from_millis(50);

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
    /// Held for the whole of a stream, so only one runs at a time.
    streaming: Mutex<()>,
    chunk_len: usize,
    /// TEST HOOK: a disconnection or change to pretend happens when the
    /// copy reaches a given offset.
    #[cfg(any(test, feature = "test-hooks"))]
    fault: Option<SimulatedFault>,
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
    /// Anything else.
    Other,
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
pub(super) fn classify(error: &io::Error) -> Failure {
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
    /// file, for errors.
    pub(super) fn new(
        original: &Path,
        origin: Origin,
        copy_folder: &TempFolder,
        chunk_len: usize,
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

        Ok(Self {
            map: OnceLock::new(),
            external: Mutex::new(Some(external)),
            copy,
            copy_path,
            len,
            copied: AtomicUsize::new(0),
            disconnected: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            streaming: Mutex::new(()),
            // A zero chunk length would never get anywhere.
            chunk_len: chunk_len.max(1),
            #[cfg(any(test, feature = "test-hooks"))]
            fault: None,
        })
    }

    /// TEST HOOK: pretend `fault` happens during the copy.
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) fn set_fault(&mut self, fault: Option<SimulatedFault>) {
        self.fault = fault;
    }

    /// TEST HOOK: the simulated fault's error, if a read of the drive up to
    /// `end` reaches it. It sets the state a real fault would.
    #[cfg(any(test, feature = "test-hooks"))]
    fn simulated_fault(&self, end: usize) -> Option<ReadError> {
        match self.fault? {
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
        if self.map.get().is_some() {
            Storage::Copy
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

    /// How many bytes, from the start, can be read: all of them, unless the
    /// drive vanished before they were copied.
    pub(super) fn available_len(&self) -> usize {
        if self.storage() == Storage::Disconnected {
            self.copied.load(Ordering::Acquire)
        } else {
            self.len
        }
    }

    /// The bytes in `range`, which the caller has already clamped to the
    /// file. Never waits (it may be on the main thread).
    pub(super) fn read_range(&self, range: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
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
        if self.disconnected.load(Ordering::Acquire) {
            return Err(ReadError::already_disconnected());
        }
        if self.changed_on_disk() {
            return Err(ReadError::changed_on_disk());
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
        let bytes =
            read_at(&external, range).map_err(|error| self.read_failed(error, Wait::Never))?;
        // Without a clone, bytes read after a change are never returned.
        self.check_unchanged(Wait::Never)?;
        Ok(Cow::Owned(bytes))
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
                self.copy_chunk(chunk, offset)?;
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
    /// copied nor delivered.
    fn copy_chunk(&self, chunk: &mut [u8], offset: usize) -> Result<(), ReadError> {
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
        external
            .read_exact_at(chunk, to_u64(offset))
            .map_err(|error| self.read_failed(error, Wait::Allowed))?;
        self.check_unchanged(Wait::Allowed)?;
        self.copy
            .write_all_at(chunk, to_u64(offset))
            .map_err(ReadError::other)
    }

    /// The copy is complete: make it read-only, map it, and drop the file on
    /// the drive (deleting the clone, its folder and its record, if it is
    /// one). The last chunk's change check was the last read of the file.
    fn switch_to_map(&self) -> Result<(), ReadError> {
        let map = (|| {
            fs::set_permissions(&self.copy_path, Permissions::from_mode(0o400))?;
            sys::map_read_only(&File::open(&self.copy_path)?)
        })()
        .map_err(ReadError::other)?;
        // Only `stream` sets the map, and only one stream runs at a time, so
        // it isn't set yet.
        let _ = self.map.set(map);
        // Taken out first, so the lock isn't held while the files are
        // deleted.
        let external = self.lock_external().take();
        drop(external);
        Ok(())
    }

    /// For the user's own file (no clone): `fstat` it and compare its
    /// inode, size and modification time with the ones at open. A change
    /// sets `changed` for good and gives [`ReadErrorKind::ChangedOnDisk`].
    /// A clone can't change, so it is always `Ok`.
    fn check_unchanged(&self, wait: Wait) -> Result<(), ReadError> {
        if self.changed_on_disk() {
            return Err(ReadError::changed_on_disk());
        }
        let now = {
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
                Ok(now) if same_contents(watched, &now) => return Ok(()),
                Ok(_) => None,
                Err(error) => Some(error),
            }
        };
        match now {
            None => {
                self.changed.store(true, Ordering::Release);
                Err(ReadError::changed_on_disk())
            }
            // `fstat` itself failed: the descriptor was revoked.
            Some(error) => Err(self.failed_with(error, Failure::Disconnected, wait)),
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
    /// drive. A short read (`UnexpectedEof`) means the user's file shrank:
    /// that is a change, not an I/O error.
    fn read_failed(&self, error: io::Error, wait: Wait) -> ReadError {
        if error.kind() == io::ErrorKind::UnexpectedEof
            && let Err(changed) = self.check_unchanged(wait)
        {
            return changed;
        }
        let failure = classify(&error);
        self.failed_with(error, failure, wait)
    }

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
            Failure::Other => false,
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

/// Whether the file `now` describes still has the size and modification
/// time it had at open (`then`), and is the same file.
fn same_contents(then: &FileIdentity, now: &fs::Metadata) -> bool {
    now.ino() == then.inode && now.len() == then.len && now.modified().ok() == then.modified
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
        classify(&io::Error::from_raw_os_error(errno))
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
        assert_eq!(classify(&eof), Failure::Other);
    }
}
