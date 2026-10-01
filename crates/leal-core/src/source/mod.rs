//! Getting a file's bytes without copying them (DESIGN §3.1).
//!
//! [`Source::open`] makes a private snapshot of the user's file and gives
//! its bytes as one `&[u8]`:
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
//! Only when the file's volume can't clone at all (`ENOTSUP`: HFS+, exFAT,
//! network shares, some USB drives) does it fall back: a file of up to
//! [`MEMORY_FALLBACK_MAX_BYTES`] is read into memory, a larger one is
//! copied to Leal's temporary directory and the copy is mapped.
//! [`Source::storage`] says which happened, so the app can show its status
//! bar note.
//!
//! The clone or copy is deleted when the [`Source`] is dropped. Each
//! temporary folder is recorded first, so [`TempFolders::remove_leftovers`]
//! can remove what a crash left behind, at the next launch.
//!
//! `open` also reads the file's raw extended attributes that later steps
//! need ([`RawAttributes`]), without interpreting them: detection (PLAN 1.2)
//! does that.

mod error;
// The one place in leal-core that may use `unsafe` (CLAUDE.md): the system
// calls the standard library doesn't wrap, and the memory map.
#[allow(unsafe_code)]
mod sys;
mod temp;
#[cfg(test)]
mod tests;

use std::ffi::CStr;
use std::fmt;
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use memmap2::Mmap;

use error::Step;
pub use error::{OpenError, OpenErrorKind};
use temp::TempFolder;
pub use temp::TempFolders;

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

/// The longest attribute value [`Source::open`] reads. Both attributes are a
/// few dozen bytes; a longer value is treated as absent.
pub const ATTRIBUTE_MAX_BYTES: usize = 64 * 1024;

/// The C-string forms of the attribute names, for `fgetxattr`.
const TEXT_ENCODING_ATTRIBUTE_C: &CStr = c"com.apple.TextEncoding";
const INTERPRETATION_ATTRIBUTE_C: &CStr = c"io.github.robhaswell.leal.interpretation";

/// An opened file's bytes: a private, read-only snapshot of the file as it
/// was when it was opened.
///
/// It is `Send` and `Sync`, so a document can share it between threads.
pub struct Source {
    // Fields are dropped in order: the map goes before the temporary
    // folder that holds the mapped file.
    bytes: Bytes,
    /// Held only so that dropping the source deletes the clone or copy
    /// (`TempFolder`'s `Drop`); tests also look inside it.
    #[cfg_attr(not(test), expect(dead_code, reason = "held for its Drop"))]
    temp: Option<TempFolder>,
    path: PathBuf,
    storage: Storage,
    attributes: RawAttributes,
    identity: FileIdentity,
}

/// Where a [`Source`]'s bytes are held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// A clone of the file, memory-mapped. The normal case.
    Clone,
    /// Read into memory, because the file's volume can't clone. The app
    /// shows a status bar note (DESIGN §3.1).
    Memory,
    /// A copy in Leal's temporary directory, memory-mapped, because the
    /// file's volume can't clone and the file is larger than
    /// [`MEMORY_FALLBACK_MAX_BYTES`].
    Copy,
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
    /// assert_eq!(source.bytes(), b"name,age\nAda,36\n");
    /// assert_eq!(source.storage(), Storage::Clone);
    ///
    /// // The snapshot doesn't change when the file does.
    /// std::fs::write(&path, "").unwrap();
    /// assert_eq!(source.bytes(), b"name,age\nAda,36\n");
    /// # drop(source);
    /// # std::fs::remove_dir_all(&dir).unwrap();
    /// ```
    pub fn open(
        path: &Path,
        temp: &TempFolders,
        volume_folder: Option<PathBuf>,
    ) -> Result<Self, OpenError> {
        Self::open_with_memory_limit(path, temp, volume_folder, MEMORY_FALLBACK_MAX_BYTES)
    }

    /// [`open`](Self::open), with the fallback's memory limit as a
    /// parameter so tests can reach the copy fallback with small files.
    fn open_with_memory_limit(
        path: &Path,
        temp: &TempFolders,
        volume_folder: Option<PathBuf>,
        memory_limit: u64,
    ) -> Result<Self, OpenError> {
        // Take over the given folder first, so it is removed on every early
        // return below.
        let volume_folder = volume_folder.map(GivenFolder);
        let (file, identity) = open_regular(path)?;
        let attributes = RawAttributes::read(&file);

        let (bytes, temp_folder, storage) = match clone(&file, path, temp, volume_folder)? {
            Some(clone) => {
                let map = map_file(path, &clone.file_path())?;
                (Bytes::Mapped(map), Some(clone), Storage::Clone)
            }
            None if identity.len <= memory_limit => {
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
        })
    }

    /// The file's bytes, exactly as they were on disk when it was opened.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match &self.bytes {
            Bytes::Mapped(map) => map,
            Bytes::Owned(bytes) => bytes,
        }
    }

    /// The path the file was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the bytes are held: a clone, memory or a copy.
    #[must_use]
    pub fn storage(&self) -> Storage {
        self.storage
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
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the bytes themselves, which may be gigabytes.
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("len", &self.bytes().len())
            .field("storage", &self.storage)
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
    // The clone gets the original's permissions. Make it read-only for
    // everyone, so nothing writes to it by accident while it is mapped. If
    // the volume doesn't support that, the clone is still private.
    let _ = fs::set_permissions(&clone, Permissions::from_mode(0o400));
    Ok(())
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
    let _ = fs::set_permissions(&copy_path, Permissions::from_mode(0o400));
    Ok(folder)
}
