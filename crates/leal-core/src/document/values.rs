//! Whole values, for **Copy** and the cell inspector (task 1.8).
//!
//! The grid reads only the start of each cell (`display_prefix`); these
//! read whole display values (`display_value`, the 1.4 notes): what the
//! inspector shows, and what Copy puts on the clipboard. Neither fixes
//! anything: an invalid byte is the U+FFFD the grid shows, and text after a
//! closing quote is copied as it reads (ADR-0002 question 6).

use std::borrow::Cow;
use std::ops::Range;
use std::sync::{Arc, Mutex, PoisonError};

use super::search::{Settled, WAIT_FOR_INDEX, rows_settled};
use super::{Document, Reading};
use crate::diagnostics::has_invalid;
use crate::index::{RowIndex, Status};
use crate::schedule::{Interval, Job, JobError, JobHandle, Priority};
use crate::source::{ReadError, Source};

/// Rows copied between checkpoints: a few milliseconds of work at most for
/// rows of ordinary width.
const COPY_CHUNK_ROWS: usize = 1024;

/// One cell's whole value, for the cell inspector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellValue {
    /// The display value, or its first `max_chars` characters.
    pub text: String,
    /// Whether the value is longer than `text`.
    pub truncated: bool,
    /// The whole value's length in characters (Unicode scalar values).
    pub characters: usize,
    /// How many lines the whole value has: line breaks (CRLF, LF or a lone
    /// CR) plus one, or 0 for an empty value.
    pub lines: usize,
    /// Whether the value has bytes that aren't valid text in the file's
    /// encoding, shown as U+FFFD.
    pub invalid: bool,
    /// Whether the row has this field at all: `false` for a short (ragged)
    /// row's missing cell, whose `text` is empty.
    pub exists: bool,
}

/// The text a copy job made. It is taken once, so a large copy is never
/// duplicated on its way to the clipboard.
#[derive(Debug, Default)]
pub struct CopiedText {
    text: Mutex<Option<String>>,
}

impl CopiedText {
    fn new(text: String) -> CopiedText {
        CopiedText {
            text: Mutex::new(Some(text)),
        }
    }

    /// The text, the first time; `None` after.
    pub fn take(&self) -> Option<String> {
        self.text
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

impl Document {
    /// Field `column` of physical row `row` in full (up to `max_chars`
    /// characters of it), with its length in characters and lines, for the
    /// cell inspector. `None` if the row can't be read yet (past the
    /// indexed rows).
    ///
    /// It decodes the whole value to count it, which for a value of many
    /// megabytes takes milliseconds: call it off the main thread.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn cell_value(
        &self,
        row: usize,
        column: usize,
        max_chars: usize,
    ) -> Result<Option<CellValue>, ReadError> {
        let encoding = self.current().detection.encoding;
        let mut values =
            self.read_rows(row..row.saturating_add(1), |parser, bytes, base, parsed| {
                let Some(field) = parsed.field(column) else {
                    return CellValue {
                        text: String::new(),
                        truncated: false,
                        characters: 0,
                        lines: 0,
                        invalid: false,
                        exists: false,
                    };
                };
                let value = parser.display_value_in(bytes, base, field);
                let raw = field
                    .start()
                    .checked_sub(base)
                    .and_then(|start| bytes.get(start..start + field.len()))
                    .unwrap_or_default();
                let characters = value.chars().count();
                let shown = value
                    .char_indices()
                    .nth(max_chars)
                    .map_or(value.len(), |(at, _)| at);
                CellValue {
                    text: value[..shown].to_owned(),
                    truncated: shown < value.len(),
                    characters,
                    lines: line_count(&value),
                    invalid: has_invalid(raw, encoding),
                    exists: true,
                }
            })?;
        Ok(values.pop())
    }

    /// Copies physical rows `rows` and fields `columns` as tab-separated
    /// text ([`push_tsv_cell`]) at once, on the calling thread, if every
    /// one of the rows is indexed (or the index is complete: rows past the
    /// file's end are left out). `None` if some aren't yet: then
    /// [`copy_cells`](Self::copy_cells) waits for them. For a selection of
    /// a few screens, this takes well under a millisecond, so Copy puts
    /// the text on the clipboard straight away.
    ///
    /// # Errors
    ///
    /// As for [`rows`](Self::rows).
    pub fn copy_cells_now(
        &self,
        rows: Range<usize>,
        columns: Range<usize>,
    ) -> Result<Option<String>, ReadError> {
        let reading = self.current();
        let indexed = reading.index.row_count();
        if rows.end > indexed && reading.index.status() != Status::Complete {
            return Ok(None);
        }
        let mut out = String::new();
        let end = rows.end.min(indexed);
        if rows.start < end {
            append_rows(
                &reading,
                &reading.index,
                FileBytes {
                    source: &self.source,
                    head: &self.head,
                },
                rows.start..end,
                rows.start,
                &columns,
                &mut out,
            )?;
        }
        Ok(Some(out))
    }

    /// About how long copying rows `rows` and fields `columns` would make
    /// the text, in bytes, so the app can ask before a very large copy:
    /// the rows' bytes in the file (past the indexed ones, the rest of the
    /// file as far as the rows reach, at the average row length so far),
    /// times the share of the columns copied. Quick: no row is read.
    #[must_use]
    pub fn estimated_copy_bytes(&self, rows: Range<usize>, columns: Range<usize>) -> u64 {
        let reading = self.current();
        let index = &reading.index;
        let indexed = index.row_count();
        let scanned = index.rows_extent(0..indexed).map_or(0, |extent| extent.end);
        let known_end = rows.end.min(indexed);
        let known = if rows.start < known_end {
            index
                .rows_extent(rows.start..known_end)
                .map_or(0, |extent| extent.len())
        } else {
            0
        };
        let rest = usize::try_from(self.source.len())
            .unwrap_or(usize::MAX)
            .saturating_sub(scanned);
        let unknown = match rows.end.saturating_sub(indexed.max(rows.start)) {
            0 => 0,
            more if indexed == 0 => rest.min(more.saturating_mul(rest)),
            more => rest.min(more.saturating_mul(scanned / indexed)),
        };
        let fields = index.field_count_mode().unwrap_or(1).max(1);
        let copied = columns.len().min(fields);
        let bytes = u128::try_from(known.saturating_add(unknown)).unwrap_or(u128::MAX);
        let share =
            bytes * u128::try_from(copied).unwrap_or(0) / u128::try_from(fields).unwrap_or(1);
        u64::try_from(share).unwrap_or(u64::MAX)
    }

    /// Copies physical rows `rows` and fields `columns` as tab-separated
    /// text ([`push_tsv_cell`]), a row per line, as a P2 job: it pauses
    /// while the user scrolls, and a selection past the indexed rows waits
    /// for the index to reach them. Rows past the file's end, once it is
    /// known, are left out; a short row's missing cells are empty.
    ///
    /// The job keeps what it reads: this reading and the file's bytes (its
    /// clone). So it finishes, with the cells as they were when it
    /// started, even if the document closes or the file is read again
    /// meanwhile: the app has promised its text to the pasteboard. If the
    /// document's index stops before reaching the rows (closing cancels
    /// it), the job indexes the file itself.
    pub fn copy_cells(&self, rows: Range<usize>, columns: Range<usize>) -> JobHandle<CopiedText> {
        let reading = self.current();
        let source = Arc::clone(&self.source);
        let head = Arc::clone(&self.head);
        self.scheduler
            .spawn(Priority::P2, Interval::Copy, move |job| {
                let mut out = String::new();
                let mut next = rows.start;
                let mut index = Arc::clone(&reading.index);
                while next < rows.end {
                    job.checkpoint()?;
                    let available = match wait_for_row(&index, &reading, next, job) {
                        Ok(available) => available,
                        // The document's index stopped (it closed, or was
                        // read again), not this job: index the file here.
                        Err(JobError::Cancelled) if !job.is_cancelled() => {
                            index = own_index(&source, &reading, job)?;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    if next >= available {
                        break;
                    }
                    let end = rows.end.min(available).min(next + COPY_CHUNK_ROWS);
                    append_rows(
                        &reading,
                        &index,
                        FileBytes {
                            source: &source,
                            head: &head,
                        },
                        next..end,
                        rows.start,
                        &columns,
                        &mut out,
                    )?;
                    next = end;
                }
                Ok(CopiedText::new(out))
            })
    }
}

/// Where a copy reads the file's bytes: the first 64 KB kept in memory, or
/// the file itself (a borrow of the map).
#[derive(Clone, Copy)]
struct FileBytes<'a> {
    source: &'a Source,
    head: &'a [u8],
}

impl FileBytes<'_> {
    fn read(&self, extent: Range<usize>) -> Result<Cow<'_, [u8]>, ReadError> {
        match self.head.get(extent.clone()) {
            Some(bytes) => Ok(Cow::Borrowed(bytes)),
            None => self.source.read_range(extent),
        }
    }
}

/// Appends rows `rows` (all indexed) of a copy that starts at row `first`
/// to `out`: a line break before each row but the first, a tab between
/// fields `columns`, and each display value as [`push_tsv_cell`] writes it.
fn append_rows(
    reading: &Reading,
    index: &RowIndex,
    file: FileBytes<'_>,
    rows: Range<usize>,
    first: usize,
    columns: &Range<usize>,
    out: &mut String,
) -> Result<(), ReadError> {
    let Some(extent) = index.rows_extent(rows.clone()) else {
        return Ok(());
    };
    let bytes = file.read(extent.clone())?;
    for row in rows {
        if row > first {
            out.push('\n');
        }
        let parsed = reading
            .parser
            .parse_row_in(index, row, &bytes, extent.start);
        for column in columns.clone() {
            if column > columns.start {
                out.push('\t');
            }
            if let Some(field) = parsed.as_ref().and_then(|p| p.field(column)) {
                let value = reading.parser.display_value_in(&bytes, extent.start, field);
                push_tsv_cell(out, &value);
            }
        }
    }
    Ok(())
}

/// Waits until row `row` is indexed, or the index is complete, and returns
/// how many rows are indexed then.
fn wait_for_row(
    index: &RowIndex,
    reading: &Reading,
    row: usize,
    job: &Job,
) -> Result<usize, JobError> {
    loop {
        let indexed = index.row_count();
        if row < indexed || index.status() == Status::Complete {
            return Ok(indexed);
        }
        match rows_settled(reading, job)? {
            Settled::Done => return Ok(reading.index.row_count()),
            Settled::Waiting => {
                job.checkpoint()?;
                std::thread::sleep(WAIT_FOR_INDEX);
            }
        }
    }
}

/// An index of the whole file, made by a copy job whose document's index
/// stopped before reaching its rows (the document closed, or the file was
/// read again). It is the same scan as the document's ([`Indexer::run`]),
/// over the map, or over the whole file read into memory if it is on a
/// removable drive that was still being copied. It checks the job's cancel
/// flag between chunks.
///
/// [`Indexer::run`]: crate::index::Indexer::run
fn own_index(source: &Source, reading: &Reading, job: &Job) -> Result<Arc<RowIndex>, JobError> {
    let (index, indexer) = RowIndex::start(reading.index.dialect())?;
    let len = usize::try_from(source.len()).unwrap_or(usize::MAX);
    let bytes = match source.as_slice() {
        Some(bytes) => Cow::Borrowed(bytes),
        None => source.read_range(0..len)?,
    };
    indexer.run(&bytes, job.cancel_flag(), |_| {})?;
    Ok(index)
}

/// Appends one cell to tab-separated text, the way spreadsheets put cells
/// on the clipboard: as it is, unless it holds a tab, a line break or a
/// quote, in which case it is quoted, with each quote doubled. So Numbers
/// and Excel paste a multiline cell back as one cell.
pub fn push_tsv_cell(out: &mut String, value: &str) {
    if !value.contains(['\t', '\n', '\r', '"']) {
        out.push_str(value);
        return;
    }
    out.push('"');
    for part in value.split_inclusive('"') {
        out.push_str(part);
        if part.ends_with('"') {
            out.push('"');
        }
    }
    out.push('"');
}

/// Line breaks (CRLF, LF or a lone CR) plus one, or 0 for empty text.
fn line_count(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let bytes = text.as_bytes();
    let breaks = memchr::memchr2_iter(b'\n', b'\r', bytes)
        .filter(|&at| !(bytes[at] == b'\n' && at > 0 && bytes[at - 1] == b'\r'))
        .count();
    breaks + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tsv(value: &str) -> String {
        let mut out = String::new();
        push_tsv_cell(&mut out, value);
        out
    }

    #[test]
    fn plain_cells_are_copied_as_they_are() {
        assert_eq!(tsv("Marlow Foods"), "Marlow Foods");
        assert_eq!(tsv(""), "");
        assert_eq!(tsv("a, b"), "a, b");
        assert_eq!(tsv("caf\u{FFFD}"), "caf\u{FFFD}");
    }

    #[test]
    fn tabs_line_breaks_and_quotes_are_quoted() {
        assert_eq!(tsv("a\tb"), "\"a\tb\"");
        assert_eq!(tsv("two\nlines"), "\"two\nlines\"");
        assert_eq!(tsv("cr\r\nlf"), "\"cr\r\nlf\"");
        assert_eq!(tsv("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(tsv("\""), "\"\"\"\"");
    }

    #[test]
    fn lines_count_each_kind_of_break_once() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("one"), 1);
        assert_eq!(line_count("a\nb\nc"), 3);
        assert_eq!(line_count("a\r\nb"), 2);
        assert_eq!(line_count("a\rb\r"), 3);
        assert_eq!(line_count("\n"), 2);
    }
}
