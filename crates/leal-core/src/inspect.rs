//! A quick look at a file: its size and first line.
//!
//! This exists for the walking skeleton (PLAN 0.3), to prove that the app can
//! call into the core. The real reader is the [`source`](crate::source)
//! module (PLAN 1.1); task 1.6 switches the app over to it and removes this.

use std::io::Read;
use std::path::Path;

use crate::source::{OpenError, open_regular};

/// The most bytes of the first line that [`inspect_file`] returns.
pub const FIRST_LINE_MAX_BYTES: usize = 200;

/// What [`inspect_file`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSummary {
    /// The file's size in bytes.
    pub byte_count: u64,
    /// The first line, without its line ending (LF, CRLF or a lone CR) and
    /// without a UTF-8 byte order mark, decoded as UTF-8. Only the file's
    /// first [`FIRST_LINE_MAX_BYTES`] bytes are looked at, so a longer line
    /// is cut. Invalid bytes become U+FFFD. A character cut in half by the
    /// limit is dropped.
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
/// Returns an [`OpenError`] if the file can't be opened or read, the same
/// way [`Source::open`](crate::source::Source::open) does: for example
/// [`OpenErrorKind::NotFound`](crate::source::OpenErrorKind::NotFound) if it
/// doesn't exist, or
/// [`OpenErrorKind::Directory`](crate::source::OpenErrorKind::Directory) if
/// `path` is a folder.
pub fn inspect_file(path: &Path) -> Result<FileSummary, OpenError> {
    let (file, identity) = open_regular(path)?;
    let byte_count = identity.len;

    let mut head = Vec::with_capacity(FIRST_LINE_MAX_BYTES);
    // `take` stops after the limit. The cast is lossless: a usize limit of 200.
    file.take(FIRST_LINE_MAX_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|error| OpenError::read(path, error))?;

    Ok(FileSummary {
        byte_count,
        first_line: first_line(&head),
    })
}

/// The UTF-8 byte order mark. It marks the encoding and belongs to no row, so
/// it isn't part of the first line.
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

/// Decodes the first line of `head`, which is the start of a file.
///
/// A line ends at LF, CRLF or a lone CR (DESIGN §3.2), so the line stops at
/// the first CR or LF. This doesn't know about quotes: a line break inside a
/// quoted first field ends the line here too.
fn first_line(head: &[u8]) -> String {
    let cut_at_limit = head.len() == FIRST_LINE_MAX_BYTES;
    let head = head.strip_prefix(UTF8_BOM).unwrap_or(head);
    let line = match head.iter().position(|&b| b == b'\n' || b == b'\r') {
        Some(end) => &head[..end],
        // No line ending: either the whole file is one line, or the line is
        // longer than the limit and `head` stops mid-line.
        None if cut_at_limit => drop_partial_char(head),
        None => head,
    };
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
    use crate::source::OpenErrorKind;
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
    fn a_lone_cr_ends_the_line() {
        let dir = TempDir::new("cr");
        let path = dir.file("a.csv", b"a,b\r1,2\r");
        assert_eq!(inspect_file(&path).unwrap().first_line, "a,b");
    }

    #[test]
    fn utf8_bom_is_not_part_of_the_line() {
        let dir = TempDir::new("bom");
        let path = dir.file("a.csv", b"\xEF\xBB\xBFa,b\n1,2\n");
        let summary = inspect_file(&path).unwrap();
        assert_eq!(summary.byte_count, 11, "the BOM still counts as file bytes");
        assert_eq!(summary.first_line, "a,b");
    }

    #[test]
    fn a_bom_alone_is_an_empty_line() {
        let dir = TempDir::new("bom-only");
        let path = dir.file("a.csv", b"\xEF\xBB\xBF");
        assert_eq!(inspect_file(&path).unwrap().first_line, "");
    }

    #[test]
    fn a_bom_only_at_the_start_is_dropped() {
        let dir = TempDir::new("bom-later");
        let path = dir.file("a.csv", "a\u{FEFF}b\n".as_bytes());
        assert_eq!(inspect_file(&path).unwrap().first_line, "a\u{FEFF}b");
    }

    #[test]
    fn long_line_after_a_bom_is_cut_at_the_limit() {
        let dir = TempDir::new("bom-long");
        // The BOM takes 3 of the 200 bytes read, then "é" straddles the end.
        let mut contents = b"\xEF\xBB\xBF".to_vec();
        contents.extend(vec![b'x'; FIRST_LINE_MAX_BYTES - 4]);
        contents.extend_from_slice("é,more".as_bytes());
        let path = dir.file("a.csv", &contents);
        let first_line = inspect_file(&path).unwrap().first_line;
        assert_eq!(first_line, "x".repeat(FIRST_LINE_MAX_BYTES - 4));
    }

    /// Files from the corpus (`tests/corpus`, task 0.2) whose first line ends
    /// in a lone CR, or that start with a UTF-8 BOM.
    #[test]
    fn corpus_files() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
        for (file, expected) in [
            ("dialect/line-endings-cr.csv", "name,qty"),
            ("dialect/line-endings-crlf.csv", "name,qty"),
            ("dialect/encoding-utf8-bom.csv", "city,population"),
            (
                "exports/imitation-excel-mac-utf8-bom.csv",
                "Name,Amount,Notes",
            ),
        ] {
            let summary = inspect_file(&corpus.join(file)).unwrap();
            assert_eq!(summary.first_line, expected, "{file}");
        }
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
        let path = dir.0.join("nope.csv");
        let err = inspect_file(&path).unwrap_err();
        assert_eq!(err.kind(), OpenErrorKind::NotFound);
        assert_eq!(err.path(), path);
        assert_eq!(err.raw_os_error(), Some(libc::ENOENT));
    }

    #[test]
    fn directory_is_an_error() {
        let dir = TempDir::new("dir");
        let err = inspect_file(&dir.0).unwrap_err();
        assert_eq!(err.kind(), OpenErrorKind::Directory);
        assert_eq!(err.raw_os_error(), Some(libc::EISDIR));
    }

    /// Opening a named pipe for reading normally waits for a writer, which
    /// would hang. It is refused at once instead.
    #[test]
    fn named_pipe_is_not_a_file() {
        let dir = TempDir::new("fifo");
        let path = dir.0.join("pipe.csv");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            inspect_file(&path).unwrap_err().kind(),
            OpenErrorKind::NotAFile
        );
    }
}
