//! Thin wrapper that exposes `leal-core` to Swift through UniFFI. It holds no
//! logic: each export calls into `leal-core` and converts the types.
//!
//! Swift bindings are generated from the built library by `just ffi` (see
//! `docs/tasks/0.3.md`). Every export that can fail, or could panic, returns
//! `Result`, so that a Rust panic reaches Swift as a thrown error rather than
//! crashing the app.

// No `unwrap`/`expect` outside tests: see `[workspace.lints.clippy]` in the
// root Cargo.toml.
#![warn(clippy::unwrap_used, clippy::expect_used)]

uniffi::setup_scaffolding!();

use std::path::{Path, PathBuf};
use std::sync::Arc;

use leal_core::source::{self, OpenError, OpenErrorKind, TempFolders};

/// Returns the version of `leal-core` this library was built with.
#[uniffi::export]
#[must_use]
pub fn core_version() -> String {
    leal_core::version().to_owned()
}

/// The size and first line of a file. See [`leal_core::FileSummary`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FileSummary {
    /// The file's size in bytes.
    pub byte_count: u64,
    /// The first line, without its line ending (LF, CRLF or a lone CR) or a
    /// UTF-8 BOM, from the first 200 bytes of the file, decoded as UTF-8 with
    /// invalid bytes replaced.
    pub first_line: String,
}

impl From<leal_core::FileSummary> for FileSummary {
    fn from(summary: leal_core::FileSummary) -> Self {
        Self {
            byte_count: summary.byte_count,
            first_line: summary.first_line,
        }
    }
}

/// An error returned to Swift, where it is thrown.
///
/// Each case carries the path that was being opened. The app words each case
/// itself (DESIGN §4.4); `code` is the OS error code (an errno, for
/// `NSError(domain: NSPOSIXErrorDomain, code:)`), and `message` is English
/// for logs only.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum LealError {
    /// No file exists at `path`.
    NotFound {
        /// The path that was opened.
        path: String,
        /// The OS error code: `ENOENT`, or `ENOTDIR` if a folder in the path
        /// is a file.
        code: Option<i32>,
    },
    /// The file exists, but Leal isn't allowed to read it.
    PermissionDenied {
        /// The path that was opened.
        path: String,
        /// The OS error code: `EACCES`, or `EPERM` from the App Sandbox.
        code: Option<i32>,
    },
    /// The path is a folder, or something else that isn't a regular file (a
    /// named pipe, socket or device).
    NotAFile {
        /// The path that was opened.
        path: String,
        /// Whether it is a folder.
        is_directory: bool,
    },
    /// Any other failure to open or read the file, or to make Leal's clone
    /// or copy of it.
    Io {
        /// The path that was opened.
        path: String,
        /// The OS error code, if the error came from the system.
        code: Option<i32>,
        /// The error in English, for logs. Not for users: the app words the
        /// error from `code`.
        message: String,
    },
}

impl LealError {
    /// Converts the core's error. `path` is the path exactly as Swift passed
    /// it.
    fn from_open(path: &str, error: &OpenError) -> Self {
        let path = path.to_owned();
        let code = error.raw_os_error();
        match error.kind() {
            OpenErrorKind::NotFound => Self::NotFound { path, code },
            OpenErrorKind::PermissionDenied => Self::PermissionDenied { path, code },
            OpenErrorKind::Directory => Self::NotAFile {
                path,
                is_directory: true,
            },
            OpenErrorKind::NotAFile => Self::NotAFile {
                path,
                is_directory: false,
            },
            OpenErrorKind::Other => Self::Io {
                path,
                code,
                message: error.to_string(),
            },
        }
    }
}

impl std::fmt::Display for LealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path, .. } => write!(f, "{path} was not found"),
            Self::PermissionDenied { path, .. } => write!(f, "no permission to read {path}"),
            Self::NotAFile {
                path,
                is_directory: true,
            } => write!(f, "{path} is a folder"),
            Self::NotAFile { path, .. } => write!(f, "{path} is not a regular file"),
            Self::Io { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for LealError {}

/// Reads the size and first line of the file at `path`.
///
/// # Errors
///
/// [`LealError::NotFound`], [`LealError::PermissionDenied`],
/// [`LealError::NotAFile`] (for example, a folder), or [`LealError::Io`] if
/// it can't be read.
#[uniffi::export]
pub fn inspect_file(path: &str) -> Result<FileSummary, LealError> {
    leal_core::inspect_file(Path::new(&path))
        .map(FileSummary::from)
        .map_err(|err| LealError::from_open(path, &err))
}

/// Where the core may put temporary files. See
/// [`leal_core::source::TempFolders`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TempLocations {
    /// The app's temporary directory (`FileManager.temporaryDirectory`):
    /// clones of files on its volume, and copies.
    pub scratch_dir: String,
    /// A folder in Application Support where Leal records each temporary
    /// folder it makes, for cleanup after a crash.
    pub records_dir: String,
}

impl From<TempLocations> for TempFolders {
    fn from(locations: TempLocations) -> Self {
        TempFolders::new(locations.scratch_dir, locations.records_dir)
    }
}

/// Where an opened file's bytes are held. See
/// [`leal_core::source::Storage`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SourceStorage {
    /// A memory-mapped clone. The normal case.
    Clone,
    /// Read into memory, because the volume can't clone. The app shows a
    /// status bar note.
    Memory,
    /// A memory-mapped copy, because the volume can't clone and the file is
    /// large, or because the file is on a removable drive and has been
    /// copied to the internal disk.
    Copy,
    /// The file is on a removable drive: read with ordinary reads, never
    /// mapped, until it has been copied to the internal disk (ADR-0006).
    Reading,
    /// The file's removable drive was disconnected before the copy was
    /// complete. What was copied can still be read; Save is refused and
    /// Save As is allowed. The app shows the "drive disconnected" banner.
    Disconnected,
}

impl From<source::Storage> for SourceStorage {
    fn from(storage: source::Storage) -> Self {
        match storage {
            source::Storage::Clone => Self::Clone,
            source::Storage::Memory => Self::Memory,
            source::Storage::Copy => Self::Copy,
            source::Storage::Reading => Self::Reading,
            source::Storage::Disconnected => Self::Disconnected,
        }
    }
}

/// What the app knows about the volume a file is on. See
/// [`leal_core::source::VolumeInfo`].
#[derive(Debug, Clone, Default, PartialEq, Eq, uniffi::Record)]
pub struct VolumeInfo {
    /// A new, empty folder on the file's own volume, from
    /// `FileManager.url(for: .itemReplacementDirectory, in: .userDomainMask,
    /// appropriateFor:, create: true)`, or `nil` if there is none. The core
    /// takes it over and deletes it.
    #[uniffi(default = None)]
    pub folder: Option<String>,
    /// The file's `URLResourceValues.volumeIsInternal`, if known.
    #[uniffi(default = None)]
    pub is_internal: Option<bool>,
    /// The file's `URLResourceValues.volumeIsEjectable`, if known.
    #[uniffi(default = None)]
    pub is_ejectable: Option<bool>,
}

impl From<VolumeInfo> for source::VolumeInfo {
    fn from(volume: VolumeInfo) -> Self {
        Self {
            folder: volume.folder.map(PathBuf::from),
            is_internal: volume.is_internal,
            is_ejectable: volume.is_ejectable,
        }
    }
}

/// An opened file: a private snapshot of its bytes. See
/// [`leal_core::source::Source`]. Its clone or copy is deleted when Swift
/// releases the last reference.
#[derive(Debug, uniffi::Object)]
pub struct Source {
    source: source::Source,
}

#[uniffi::export]
impl Source {
    /// The file's size in bytes when it was opened.
    #[must_use]
    pub fn byte_count(&self) -> u64 {
        self.source.len()
    }

    /// Where the bytes are held.
    #[must_use]
    pub fn storage(&self) -> SourceStorage {
        self.source.storage().into()
    }
}

/// Opens the file at `path` and takes a snapshot of it. See
/// [`leal_core::source::Source::open_on`].
///
/// `volume` is what Foundation says about the file's volume: a folder on it
/// for the clone, and whether it is internal and ejectable, which decides
/// whether the file is treated as being on a removable drive (ADR-0006).
///
/// # Errors
///
/// [`LealError::NotFound`], [`LealError::PermissionDenied`],
/// [`LealError::NotAFile`], or [`LealError::Io`].
#[uniffi::export]
pub fn open_source(
    path: &str,
    volume: VolumeInfo,
    temp: TempLocations,
) -> Result<Arc<Source>, LealError> {
    let temp = TempFolders::from(temp);
    source::Source::open_on(Path::new(path), &temp, volume.into())
        .map(|source| Arc::new(Source { source }))
        .map_err(|err| LealError::from_open(path, &err))
}

/// Removes the temporary folders that crashed Leal processes left behind.
/// The app calls it at launch. Returns how many it removed. See
/// [`leal_core::source::TempFolders::remove_leftovers`].
///
/// # Errors
///
/// [`LealError::Io`] if the records folder can't be listed.
#[uniffi::export]
pub fn remove_leftover_temp_folders(temp: TempLocations) -> Result<u32, LealError> {
    let records = temp.records_dir.clone();
    let removed = TempFolders::from(temp)
        .remove_leftovers()
        .map_err(|err| LealError::Io {
            message: format!("couldn't list {records}: {err}"),
            code: err.raw_os_error(),
            path: records,
        })?;
    Ok(u32::try_from(removed).unwrap_or(u32::MAX))
}

/// Panics with `message`. It exists so the app's tests can check that a Rust
/// panic in an export that returns `Result` reaches Swift as a thrown error
/// instead of crashing (`testRustPanicThrowsInsteadOfCrashing`).
///
/// Only built with the `test-exports` feature, so it is never in the release
/// library the app ships with.
///
/// # Errors
///
/// Never returns an error.
///
/// # Panics
///
/// Always.
#[cfg(feature = "test-exports")]
#[uniffi::export]
pub fn debug_panic(message: &str) -> Result<(), LealError> {
    panic!("{message}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A temporary directory that is deleted when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("leal-ffi-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> String {
            self.0.join(name).to_string_lossy().into_owned()
        }

        fn locations(&self) -> TempLocations {
            TempLocations {
                scratch_dir: self.path("scratch"),
                records_dir: self.path("records"),
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn core_version_comes_from_core() {
        assert_eq!(core_version(), leal_core::version());
    }

    #[test]
    fn inspect_file_wraps_core() {
        let dir = TempDir::new("inspect");
        let path = dir.path("a.csv");
        std::fs::write(&path, b"a,b\n1,2\n").unwrap();
        assert_eq!(
            inspect_file(&path),
            Ok(FileSummary {
                byte_count: 8,
                first_line: "a,b".to_owned(),
            })
        );
    }

    #[test]
    fn missing_file_is_not_found() {
        let path = "/nonexistent/leal/missing.csv".to_owned();
        assert_eq!(
            inspect_file(&path),
            Err(LealError::NotFound {
                path,
                // ENOENT
                code: Some(2)
            })
        );
    }

    #[test]
    fn directory_is_not_a_file() {
        let dir = TempDir::new("directory");
        let path = dir.path("");
        assert_eq!(
            inspect_file(&path),
            Err(LealError::NotAFile {
                path: path.clone(),
                is_directory: true
            })
        );
        assert!(matches!(
            open_source(&path, VolumeInfo::default(), dir.locations()),
            Err(LealError::NotAFile {
                is_directory: true,
                ..
            })
        ));
    }

    #[test]
    fn permission_denied() {
        let dir = TempDir::new("denied");
        let path = dir.path("locked.csv");
        std::fs::write(&path, b"a\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let expected = LealError::PermissionDenied {
            path: path.clone(),
            // EACCES
            code: Some(13),
        };
        assert_eq!(inspect_file(&path), Err(expected.clone()));
        assert_eq!(
            open_source(&path, VolumeInfo::default(), dir.locations()).map(|_| ()),
            Err(expected)
        );
    }

    #[test]
    fn other_errors_carry_the_code_and_a_log_message() {
        let dir = TempDir::new("other");
        let path = dir.path("a.csv");
        std::fs::write(&path, b"a\n").unwrap();
        // The scratch "directory" is a file, so making a folder in it fails.
        std::fs::write(dir.path("scratch"), b"").unwrap();
        let Err(LealError::Io {
            path: error_path,
            code,
            message,
        }) = open_source(&path, VolumeInfo::default(), dir.locations())
        else {
            panic!("expected LealError::Io");
        };
        assert_eq!(error_path, path);
        assert!(code.is_some());
        assert!(message.contains("a.csv"), "{message}");
    }

    #[test]
    fn open_source_wraps_core() {
        let dir = TempDir::new("open");
        let path = dir.path("a.csv");
        std::fs::write(&path, b"a,b\n1,2\n").unwrap();
        let folder = dir.path("volume-folder");
        std::fs::create_dir(&folder).unwrap();
        let volume = VolumeInfo {
            folder: Some(folder.clone()),
            // On the scratch directory's volume, so still mapped.
            is_internal: Some(false),
            is_ejectable: Some(true),
        };
        let source = open_source(&path, volume, dir.locations()).unwrap();
        assert_eq!(source.byte_count(), 8);
        assert_eq!(source.storage(), SourceStorage::Clone);
        drop(source);
        assert!(
            !Path::new(&folder).exists(),
            "the clone's folder is deleted"
        );
        assert_eq!(remove_leftover_temp_folders(dir.locations()), Ok(0));
    }

    #[test]
    fn volume_info_and_storage_convert() {
        let volume = source::VolumeInfo::from(VolumeInfo {
            folder: Some("/Volumes/USB/.TemporaryItems/x".to_owned()),
            is_internal: None,
            is_ejectable: Some(true),
        });
        assert_eq!(
            volume,
            source::VolumeInfo {
                folder: Some(PathBuf::from("/Volumes/USB/.TemporaryItems/x")),
                is_internal: None,
                is_ejectable: Some(true),
            }
        );
        for (core, ffi) in [
            (source::Storage::Clone, SourceStorage::Clone),
            (source::Storage::Memory, SourceStorage::Memory),
            (source::Storage::Copy, SourceStorage::Copy),
            (source::Storage::Reading, SourceStorage::Reading),
            (source::Storage::Disconnected, SourceStorage::Disconnected),
        ] {
            assert_eq!(SourceStorage::from(core), ffi);
        }
    }

    #[cfg(feature = "test-exports")]
    #[test]
    #[should_panic(expected = "deliberate")]
    fn debug_panic_panics() {
        let _ = debug_panic("deliberate");
    }
}
