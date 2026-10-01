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

use std::io;
use std::path::Path;

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
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum LealError {
    /// No file exists at `path`.
    NotFound {
        /// The path that was looked up.
        path: String,
    },
    /// The file exists but couldn't be read.
    Io {
        /// The path that was read.
        path: String,
        /// The operating system's description of the error.
        message: String,
    },
}

impl LealError {
    fn from_io(path: &str, err: &io::Error) -> Self {
        match err.kind() {
            io::ErrorKind::NotFound => Self::NotFound {
                path: path.to_owned(),
            },
            _ => Self::Io {
                path: path.to_owned(),
                message: err.to_string(),
            },
        }
    }
}

impl std::fmt::Display for LealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { path } => write!(f, "{path} was not found"),
            Self::Io { path, message } => write!(f, "couldn't read {path}: {message}"),
        }
    }
}

impl std::error::Error for LealError {}

/// Reads the size and first line of the file at `path`.
///
/// # Errors
///
/// [`LealError::NotFound`] if nothing exists at `path`, and [`LealError::Io`]
/// if it can't be read (for example, it is a directory).
#[uniffi::export]
pub fn inspect_file(path: &str) -> Result<FileSummary, LealError> {
    leal_core::inspect_file(Path::new(&path))
        .map(FileSummary::from)
        .map_err(|err| LealError::from_io(path, &err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_comes_from_core() {
        assert_eq!(core_version(), leal_core::version());
    }

    #[test]
    fn inspect_file_wraps_core() {
        let path =
            std::env::temp_dir().join(format!("leal-ffi-inspect-{}.csv", std::process::id()));
        std::fs::write(&path, b"a,b\n1,2\n").unwrap();
        let summary = inspect_file(&path.to_string_lossy());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            summary,
            Ok(FileSummary {
                byte_count: 8,
                first_line: "a,b".to_owned(),
            })
        );
    }

    #[test]
    fn missing_file_is_not_found() {
        let path = "/nonexistent/leal/missing.csv".to_owned();
        assert_eq!(inspect_file(&path), Err(LealError::NotFound { path }));
    }

    #[test]
    fn directory_is_an_io_error() {
        let path = std::env::temp_dir().to_string_lossy().into_owned();
        assert!(matches!(inspect_file(&path), Err(LealError::Io { .. })));
    }
}
