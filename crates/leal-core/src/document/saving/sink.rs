//! Where the writer's output goes: the new file ([`FileSink`]), or, for
//! tests, a list of the splices ([`Collect`]).

use std::io::Write;
use std::ops::Range;
use std::sync::atomic::Ordering;

use super::{Extent, SaveShared, Sink, Streamed, chunk_bytes, to_u64};
use crate::detect::{CensusStream, FIRST_PAINT_BYTES};
use crate::dialect::Encoding;
use crate::document::{Document, Reading};
use crate::edit::RowMap;
use crate::index::MAX_FILE_BYTES;
use crate::save::{MAX_NAMED_CELLS, SaveError, Splice, Transcoder};

/// A [`Sink`] that writes the new file: the snapshot a chunk at a time,
/// with a checkpoint before each, telling the progress, keeping the first
/// 64 KB for the attributes and feeding the census. For Save As UTF-8 from
/// another encoding it converts the snapshot's bytes as it copies them
/// ([`Transcoder`]); bytes that aren't text name their cells, and from the
/// first such it writes nothing more, but reads on to name the rest.
pub(super) struct FileSink<'a, W: Write> {
    reading: &'a Reading,
    /// The snapshot's piece list, to name a cell by its logical row.
    map: &'a RowMap,
    stale: bool,
    out: W,
    progress: &'a SaveShared,
    census: Option<&'a mut CensusStream>,
    checkpoint: &'a dyn Fn() -> Result<(), SaveError>,
    head: Vec<u8>,
    written: u64,
    /// The document's encoding, for a refusal.
    encoding: Encoding,
    /// Converting to UTF-8.
    transcoder: Option<Transcoder>,
    /// The converted chunk, its buffer kept between chunks.
    converted: Vec<u8>,
    /// The cells that can't be converted, in file order.
    unconvertible: Vec<(usize, usize)>,
    /// The span of the field last named, so its other bad bytes name it
    /// only once.
    last_named: Option<Range<usize>>,
    /// Something can't be converted: nothing more is written.
    refused: bool,
}

impl<'a, W: Write> FileSink<'a, W> {
    pub(super) fn new(
        reading: &'a Reading,
        map: &'a RowMap,
        extent: &Extent<'_>,
        out: W,
        progress: &'a SaveShared,
        census: Option<&'a mut CensusStream>,
        checkpoint: &'a dyn Fn() -> Result<(), SaveError>,
    ) -> Self {
        let transcoder = if extent.converts() {
            Transcoder::new(extent.source)
        } else {
            None
        };
        FileSink {
            reading,
            map,
            stale: reading.head_is_stale(),
            out,
            progress,
            census,
            checkpoint,
            head: Vec::new(),
            written: 0,
            encoding: extent.source,
            transcoder,
            converted: Vec::new(),
            unconvertible: Vec::new(),
            last_named: None,
            refused: false,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), SaveError> {
        if self.refused {
            return Ok(());
        }
        if self.transcoder.is_some() {
            // Converting changes every stretch's length, so the limit is
            // checked on what is written (ADR-0012 decision 2).
            let len = self.written + to_u64(bytes.len());
            if len > to_u64(MAX_FILE_BYTES) {
                return Err(SaveError::TooLarge { len });
            }
        }
        self.out
            .write_all(bytes)
            .map_err(|error| SaveError::Write {
                step: "writing the new file",
                error,
            })?;
        if self.head.len() < FIRST_PAINT_BYTES {
            let take = bytes.len().min(FIRST_PAINT_BYTES - self.head.len());
            self.head.extend_from_slice(&bytes[..take]);
        }
        if let Some(census) = &mut self.census {
            census.push(bytes);
        }
        self.written += to_u64(bytes.len());
        if self.transcoder.is_none() {
            self.progress.written.store(self.written, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Names `cell` as one that can't be converted, unless it is the last
    /// one named. Stops the save once more than [`MAX_NAMED_CELLS`] are.
    fn name(&mut self, cell: (usize, usize)) -> Result<(), SaveError> {
        self.refused = true;
        if self.unconvertible.last() == Some(&cell) {
            return Ok(());
        }
        if self.unconvertible.len() == MAX_NAMED_CELLS {
            return Err(SaveError::Unconvertible {
                encoding: self.encoding,
                cells: std::mem::take(&mut self.unconvertible),
                more: true,
            });
        }
        self.unconvertible.push(cell);
        Ok(())
    }

    /// The bytes at `offset` aren't text: names their cell.
    fn bad_at(&mut self, offset: usize) -> Result<(), SaveError> {
        self.refused = true;
        if self
            .last_named
            .as_ref()
            .is_some_and(|span| span.contains(&offset))
        {
            return Ok(());
        }
        match cell_at(self.reading, offset) {
            Some(((row, column), span)) => {
                self.last_named = Some(span);
                // A row copied isn't deleted.
                let row = u32::try_from(row)
                    .ok()
                    .and_then(|row| self.map.logical_of(row).ok())
                    .unwrap_or(row);
                self.name((row, column))
            }
            // In no row (a file that is only a broken BOM): the save is
            // still refused.
            None => Ok(()),
        }
    }

    /// The first 64 KB written, once the file is; refused if anything
    /// can't be converted. Checks that it wrote `streamed.len` bytes, or,
    /// converting (when the length wasn't known before), sets it.
    pub(super) fn finish(self, streamed: &mut Streamed) -> Result<Vec<u8>, SaveError> {
        if self.refused {
            let mut cells = self.unconvertible;
            cells.sort_unstable();
            cells.dedup();
            return Err(SaveError::Unconvertible {
                encoding: self.encoding,
                cells,
                more: false,
            });
        }
        if self.transcoder.is_some() {
            streamed.len = self.written;
        } else if self.written != streamed.len {
            return Err(SaveError::Failed(format!(
                "wrote {} bytes, not {}",
                self.written, streamed.len
            )));
        }
        Ok(self.head)
    }
}

impl<W: Write> Sink for FileSink<'_, W> {
    fn copy(&mut self, range: Range<usize>) -> Result<(), SaveError> {
        let mut at = range.start;
        while at < range.end {
            (self.checkpoint)()?;
            let to = range.end.min(at + chunk_bytes());
            let bytes = self.reading.bytes_of(at..to, self.stale)?;
            if bytes.len() != to - at {
                // The snapshot is shorter than its index says: a bug.
                return Err(SaveError::Failed(format!(
                    "the snapshot has {} bytes at {at}, not {}",
                    bytes.len(),
                    to - at
                )));
            }
            if let Some(transcoder) = &mut self.transcoder {
                // A stretch between splices ends on a whole character:
                // splices start and end at fields and rows.
                let mut converted = std::mem::take(&mut self.converted);
                converted.clear();
                let mut bad = Vec::new();
                transcoder.push(&bytes, at, to == range.end, &mut converted, &mut |offset| {
                    bad.push(offset);
                });
                for offset in bad {
                    self.bad_at(offset)?;
                }
                self.put(&converted)?;
                self.converted = converted;
                self.progress.written.store(to_u64(to), Ordering::Relaxed);
            } else {
                self.put(&bytes)?;
            }
            at = to;
        }
        Ok(())
    }

    fn splice(&mut self, splice: Splice) -> Result<(), SaveError> {
        self.put(&splice.bytes)
    }

    fn refuse(&mut self, cells: &[(usize, usize)]) -> Result<(), SaveError> {
        self.refused = true;
        for &cell in cells {
            self.name(cell)?;
        }
        Ok(())
    }
}

/// The cell (row, column) whose bytes hold `offset`, and its field's span:
/// for naming a cell that can't be converted.
fn cell_at(reading: &Reading, offset: usize) -> Option<((usize, usize), Range<usize>)> {
    let stale = reading.head_is_stale();
    let (index, _) = reading.rows_from(&reading.index, stale);
    let row = index.row_at_offset(offset)?;
    let bytes = Document::row_bytes(reading, row).ok()??;
    let parsed = reading
        .parser
        .parse_row_in(bytes.index, row, &bytes.bytes, bytes.base)?;
    let fields = parsed.fields();
    let column = fields
        .iter()
        .position(|field| field.span().contains(&offset))
        .or_else(|| fields.iter().rposition(|field| field.start() <= offset))?;
    Some(((row, column), fields[column].span()))
}

/// A [`Sink`] that only lists the splices (tests).
#[cfg(any(test, feature = "test-hooks"))]
#[derive(Default)]
pub(super) struct Collect {
    pub(super) splices: Vec<Splice>,
    /// Cells whose row's splices couldn't be made (Save As UTF-8).
    pub(super) refused: Vec<(usize, usize)>,
}

#[cfg(any(test, feature = "test-hooks"))]
impl Sink for Collect {
    fn copy(&mut self, _range: Range<usize>) -> Result<(), SaveError> {
        Ok(())
    }

    fn splice(&mut self, splice: Splice) -> Result<(), SaveError> {
        self.splices.push(splice);
        Ok(())
    }

    fn refuse(&mut self, cells: &[(usize, usize)]) -> Result<(), SaveError> {
        self.refused.extend_from_slice(cells);
        Ok(())
    }
}
