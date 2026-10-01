//! Why a file couldn't be opened or read, in a form the app can word for
//! users.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

/// Why [`Source::open`](super::Source::open) (or
/// [`inspect_file`](crate::inspect_file)) failed.
///
/// It carries the path of the file being opened, a [`kind`](Self::kind) the
/// app words in its alerts, and the underlying [`io::Error`] with its OS
/// error code ([`raw_os_error`](Self::raw_os_error), an errno). The
/// [`Display`](fmt::Display) text is English and meant for logs, not users.
#[derive(Debug)]
pub struct OpenError {
    path: PathBuf,
    kind: OpenErrorKind,
    step: Step,
    error: io::Error,
}

/// The kinds of [`OpenError`] the app words differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenErrorKind {
    /// Nothing exists at the path (`ENOENT`), or a folder in the path is a
    /// file (`ENOTDIR`).
    NotFound,
    /// The file exists, but Leal isn't allowed to read it (`EACCES`, or
    /// `EPERM`, which the App Sandbox gives).
    PermissionDenied,
    /// The path is a folder. The error code is `EISDIR`.
    Directory,
    /// The path is something other than a regular file or a folder: a named
    /// pipe, a socket or a device. There is no error code.
    NotAFile,
    /// Anything else, such as an I/O error or a full disk. The app words
    /// these from the error code.
    Other,
}

/// What `open` was doing when it failed. Only used in the log message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Opening the user's file and checking what it is.
    Open,
    /// Reading the user's file (into memory, or to copy it).
    Read,
    /// Making or recording a temporary folder, or cloning into it.
    Clone,
    /// Writing the fallback copy.
    Copy,
    /// Mapping the clone or copy.
    Map,
}

impl OpenError {
    /// An error at `step` while opening `path`.
    ///
    /// Only errors about the user's own file ([`Step::Open`] and
    /// [`Step::Read`]) get a specific [`OpenErrorKind`]. A "permission
    /// denied" on Leal's temporary folder, say, is [`OpenErrorKind::Other`],
    /// so the app doesn't tell the user they can't read their file.
    pub(crate) fn new(path: &Path, step: Step, error: io::Error) -> Self {
        let kind = match step {
            Step::Open | Step::Read => match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => OpenErrorKind::NotFound,
                io::ErrorKind::PermissionDenied => OpenErrorKind::PermissionDenied,
                io::ErrorKind::IsADirectory => OpenErrorKind::Directory,
                _ => OpenErrorKind::Other,
            },
            Step::Clone | Step::Copy | Step::Map => OpenErrorKind::Other,
        };
        Self {
            path: path.to_owned(),
            kind,
            step,
            error,
        }
    }

    /// Reading the user's file at `path` failed.
    pub(crate) fn read(path: &Path, error: io::Error) -> Self {
        Self::new(path, Step::Read, error)
    }

    /// `path` is a folder.
    pub(crate) fn directory(path: &Path) -> Self {
        Self::new(path, Step::Open, io::Error::from_raw_os_error(libc::EISDIR))
    }

    /// `path` is neither a regular file nor a folder.
    pub(crate) fn not_a_file(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            kind: OpenErrorKind::NotAFile,
            step: Step::Open,
            error: io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"),
        }
    }

    /// The path of the file that was being opened (as given, not resolved).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What kind of failure this is.
    #[must_use]
    pub fn kind(&self) -> OpenErrorKind {
        self.kind
    }

    /// The OS error code (an errno such as `libc::EACCES`), if the error came
    /// from the operating system. `None` for [`OpenErrorKind::NotAFile`].
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.error.raw_os_error()
    }

    /// The underlying I/O error.
    #[must_use]
    pub fn io_error(&self) -> &io::Error {
        &self.error
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let doing = match self.step {
            Step::Open => "open",
            Step::Read => "read",
            Step::Clone => "clone",
            Step::Copy => "copy",
            Step::Map => "map the clone of",
        };
        write!(
            f,
            "couldn't {doing} {}: {}",
            self.path.display(),
            self.error
        )
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Why [`Source::read_range`](super::Source::read_range) or
/// [`Source::stream`](super::Source::stream) failed.
///
/// Reads can only fail for a file on a removable drive, before its copy on
/// the internal disk is complete (ADR-0006), or when a stream is cancelled
/// or finds the file changed on disk.
/// The [`Display`](fmt::Display) text is English and meant for logs.
#[derive(Debug)]
pub struct ReadError {
    kind: ReadErrorKind,
    error: io::Error,
}

/// The kinds of [`ReadError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadErrorKind {
    /// The drive holding the file was disconnected (unplugged or
    /// force-ejected) before the bytes asked for were copied. The source
    /// is now [`Storage::Disconnected`](super::Storage::Disconnected): the
    /// bytes copied before it vanished can still be read, nothing after
    /// them can, and Save is refused.
    Disconnected,
    /// The stream's cancel flag was set.
    Cancelled,
    /// The pass is complete and the internal copy is mapped, but the
    /// user's file changed while it was being read: its size or
    /// modification time differ from when it was opened. This can only
    /// happen on a removable drive that can't clone, where Leal reads the
    /// user's file itself, so the bytes may mix old and new contents.
    /// [`Source::changed_on_disk`](super::Source::changed_on_disk) stays
    /// `true`, for the "changed elsewhere" banner (task 1.9).
    ChangedOnDisk,
    /// Anything else, such as a read error on a drive that is still there,
    /// or the internal disk being full while copying. The source is
    /// unchanged, so the read or stream can be tried again.
    Other,
}

impl ReadError {
    pub(crate) fn new(kind: ReadErrorKind, error: io::Error) -> Self {
        Self { kind, error }
    }

    /// The drive vanished; `error` is what the read reported.
    pub(crate) fn disconnected(error: io::Error) -> Self {
        Self::new(ReadErrorKind::Disconnected, error)
    }

    /// The drive vanished earlier, so this read wasn't tried.
    pub(crate) fn already_disconnected() -> Self {
        Self::disconnected(io::Error::new(
            io::ErrorKind::NotConnected,
            "the drive holding the file was disconnected",
        ))
    }

    pub(crate) fn cancelled() -> Self {
        Self::new(
            ReadErrorKind::Cancelled,
            io::Error::new(io::ErrorKind::Interrupted, "cancelled"),
        )
    }

    pub(crate) fn changed_on_disk() -> Self {
        Self::new(
            ReadErrorKind::ChangedOnDisk,
            io::Error::other("the file changed on disk while it was being read"),
        )
    }

    pub(crate) fn other(error: io::Error) -> Self {
        Self::new(ReadErrorKind::Other, error)
    }

    /// What kind of failure this is.
    #[must_use]
    pub fn kind(&self) -> ReadErrorKind {
        self.kind
    }

    /// The OS error code (an errno such as `libc::EIO`), if the error came
    /// from the operating system.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.error.raw_os_error()
    }

    /// The underlying I/O error.
    #[must_use]
    pub fn io_error(&self) -> &io::Error {
        &self.error
    }
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ReadErrorKind::Disconnected => write!(f, "the drive was disconnected: {}", self.error),
            ReadErrorKind::Cancelled => f.write_str("cancelled"),
            ReadErrorKind::ChangedOnDisk => f.write_str(
                "the file changed on disk while it was being read, so the copy may mix old and new bytes",
            ),
            ReadErrorKind::Other => write!(f, "couldn't read the file: {}", self.error),
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(step: Step, errno: i32) -> OpenErrorKind {
        OpenError::new(
            Path::new("/x.csv"),
            step,
            io::Error::from_raw_os_error(errno),
        )
        .kind()
    }

    #[test]
    fn errors_about_the_users_file_are_classified() {
        for step in [Step::Open, Step::Read] {
            assert_eq!(kind_of(step, libc::ENOENT), OpenErrorKind::NotFound);
            assert_eq!(kind_of(step, libc::ENOTDIR), OpenErrorKind::NotFound);
            assert_eq!(kind_of(step, libc::EACCES), OpenErrorKind::PermissionDenied);
            assert_eq!(kind_of(step, libc::EPERM), OpenErrorKind::PermissionDenied);
            assert_eq!(kind_of(step, libc::EISDIR), OpenErrorKind::Directory);
            assert_eq!(kind_of(step, libc::EIO), OpenErrorKind::Other);
        }
    }

    #[test]
    fn errors_about_temporary_files_are_other() {
        for step in [Step::Clone, Step::Copy, Step::Map] {
            for errno in [libc::ENOENT, libc::EACCES, libc::EISDIR, libc::ENOSPC] {
                assert_eq!(
                    kind_of(step, errno),
                    OpenErrorKind::Other,
                    "{step:?} {errno}"
                );
            }
        }
    }

    #[test]
    fn keeps_path_and_code() {
        let error = OpenError::new(
            Path::new("/data/a.csv"),
            Step::Copy,
            io::Error::from_raw_os_error(libc::ENOSPC),
        );
        assert_eq!(error.path(), Path::new("/data/a.csv"));
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert!(
            error.to_string().starts_with("couldn't copy /data/a.csv: "),
            "{error}"
        );
    }

    #[test]
    fn directory_and_not_a_file() {
        let directory = OpenError::directory(Path::new("/d"));
        assert_eq!(directory.kind(), OpenErrorKind::Directory);
        assert_eq!(directory.raw_os_error(), Some(libc::EISDIR));
        let fifo = OpenError::not_a_file(Path::new("/p"));
        assert_eq!(fifo.kind(), OpenErrorKind::NotAFile);
        assert_eq!(fifo.raw_os_error(), None);
    }
}
