//! A quick look at a file: its size and first line.
//!
//! This exists for the walking skeleton (PLAN 0.3), to prove that the app can
//! call into the core. The real reader is the `source` module (PLAN 1.1).

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

/// The most bytes of the first line that [`inspect_file`] returns.
pub const FIRST_LINE_MAX_BYTES: usize = 200;

/// What [`inspect_file`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    /// The file's size in bytes.
    pub byte_count: u64,
    /// The first line, without its line ending, cut to at most
    /// [`FIRST_LINE_MAX_BYTES`] bytes and decoded as UTF-8. Invalid bytes
    /// become U+FFFD. A character cut in half by the limit is dropped.
    pub first_line: String,
}

/// Reads the size and the start of the first line of the file at `path`.
///
/// Only the first [`FIRST_LINE_MAX_BYTES`] bytes are read, however large the
/// file is.
///
/// ```
/// # let dir = std::env::temp_dir().join(format!("leal-doctest-{}", std::process::id()));
/// # std::fs::create_dir_all(&dir).unwrap();
/// let path = dir.join("people.csv");
/// std::fs::write(&path, "name,age\nAda,36\n").unwrap();
///
/// let summary = leal_core::inspect_file(&path).unwrap();
/// assert_eq!(summary.byte_count, 16);
/// assert_eq!(summary.first_line, "name,age");
/// # std::fs::remove_dir_all(&dir).unwrap();
/// ```
///
/// # Errors
///
/// Returns the I/O error if the file can't be opened or read, for example
/// [`io::ErrorKind::NotFound`] if it doesn't exist, or an error if `path` is a
/// directory.
pub fn inspect_file(path: &Path) -> io::Result<FileSummary> {
    let file = File::open(path)?;
    let byte_count = file.metadata()?.len();

    let mut head = Vec::with_capacity(FIRST_LINE_MAX_BYTES);
    // `take` stops after the limit. The cast is lossless: a usize limit of 200.
    file.take(FIRST_LINE_MAX_BYTES as u64)
        .read_to_end(&mut head)?;

    Ok(FileSummary {
        byte_count,
        first_line: first_line(&head),
    })
}

/// Decodes the first line of `head`, which is the start of a file.
fn first_line(head: &[u8]) -> String {
    let line = match head.iter().position(|&b| b == b'\n') {
        Some(end) => &head[..end],
        // No line ending: either the whole file is one line, or the line is
        // longer than the limit and `head` stops mid-line.
        None if head.len() == FIRST_LINE_MAX_BYTES => drop_partial_char(head),
        None => head,
    };
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    String::from_utf8_lossy(line).into_owned()
}

/// Drops a UTF-8 character that was cut in half at the end of `bytes`, so it
/// doesn't show up as U+FFFD. Anything else is left for the lossy decode.
fn drop_partial_char(bytes: &[u8]) -> &[u8] {
    // A UTF-8 character is a lead byte followed by up to 3 continuation bytes
    // (0b10xx_xxxx). Find where the last character starts.
    let is_continuation = |b: u8| b & 0b1100_0000 == 0b1000_0000;
    let Some(start) = bytes.iter().rposition(|&b| !is_continuation(b)) else {
        return bytes;
    };
    // The lead byte says how long the character should be.
    let expected_len = match bytes[start] {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    };
    if bytes.len() - start < expected_len {
        &bytes[..start]
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temporary directory that is deleted when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("leal-core-inspect-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_size_and_first_line() {
        let dir = TempDir::new("basic");
        let path = dir.file("a.csv", b"name,age\nAda,36\n");
        let summary = inspect_file(&path).unwrap();
        assert_eq!(
            summary,
            FileSummary {
                byte_count: 16,
                first_line: "name,age".to_owned(),
            }
        );
    }

    #[test]
    fn strips_crlf() {
        let dir = TempDir::new("crlf");
        let path = dir.file("a.csv", b"a,b\r\n1,2\r\n");
        assert_eq!(inspect_file(&path).unwrap().first_line, "a,b");
    }

    #[test]
    fn file_without_newline_is_one_line() {
        let dir = TempDir::new("no-newline");
        let path = dir.file("a.csv", b"a,b");
        assert_eq!(inspect_file(&path).unwrap().first_line, "a,b");
    }

    #[test]
    fn empty_file() {
        let dir = TempDir::new("empty");
        let path = dir.file("a.csv", b"");
        let summary = inspect_file(&path).unwrap();
        assert_eq!(summary.byte_count, 0);
        assert_eq!(summary.first_line, "");
    }

    #[test]
    fn long_first_line_is_cut_at_the_limit() {
        let dir = TempDir::new("long");
        let mut contents = vec![b'x'; 1000];
        contents.push(b'\n');
        let path = dir.file("a.csv", &contents);
        let summary = inspect_file(&path).unwrap();
        assert_eq!(summary.byte_count, 1001);
        assert_eq!(summary.first_line, "x".repeat(FIRST_LINE_MAX_BYTES));
    }

    #[test]
    fn character_cut_by_the_limit_is_dropped() {
        let dir = TempDir::new("cut-char");
        // 199 ASCII bytes, then "é" (2 bytes) straddles the 200-byte limit.
        let mut contents = vec![b'x'; FIRST_LINE_MAX_BYTES - 1];
        contents.extend_from_slice("é".as_bytes());
        let path = dir.file("a.csv", &contents);
        let first_line = inspect_file(&path).unwrap().first_line;
        assert_eq!(first_line, "x".repeat(FIRST_LINE_MAX_BYTES - 1));
    }

    #[test]
    fn character_cut_by_the_limit_is_dropped_for_every_length() {
        for ch in ["é", "€", "𝄞"] {
            for kept in 1..ch.len() {
                let dir = TempDir::new(&format!("cut-{}-{kept}", ch.len()));
                let mut contents = vec![b'x'; FIRST_LINE_MAX_BYTES - kept];
                contents.extend_from_slice(ch.as_bytes());
                let path = dir.file("a.csv", &contents);
                let first_line = inspect_file(&path).unwrap().first_line;
                assert_eq!(first_line, "x".repeat(FIRST_LINE_MAX_BYTES - kept));
            }
        }
    }

    #[test]
    fn character_ending_exactly_at_the_limit_is_kept() {
        let dir = TempDir::new("whole-char");
        let mut contents = vec![b'x'; FIRST_LINE_MAX_BYTES - 2];
        contents.extend_from_slice("é,more".as_bytes());
        let path = dir.file("a.csv", &contents);
        let first_line = inspect_file(&path).unwrap().first_line;
        assert_eq!(
            first_line,
            format!("{}é", "x".repeat(FIRST_LINE_MAX_BYTES - 2))
        );
    }

    #[test]
    fn invalid_utf8_at_the_end_of_a_short_line_is_replaced() {
        let dir = TempDir::new("latin1-end");
        let path = dir.file("a.csv", b"caf\xe9");
        assert_eq!(inspect_file(&path).unwrap().first_line, "caf\u{FFFD}");
    }

    #[test]
    fn invalid_utf8_is_replaced() {
        let dir = TempDir::new("latin1");
        // "café" in Latin-1: 0xE9 is not valid UTF-8 on its own.
        let path = dir.file("a.csv", b"caf\xe9\nx\n");
        assert_eq!(inspect_file(&path).unwrap().first_line, "caf\u{FFFD}");
    }

    #[test]
    fn missing_file_is_not_found() {
        let dir = TempDir::new("missing");
        let err = inspect_file(&dir.0.join("nope.csv")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn directory_is_an_error() {
        let dir = TempDir::new("dir");
        assert!(inspect_file(&dir.0).is_err());
    }
}
