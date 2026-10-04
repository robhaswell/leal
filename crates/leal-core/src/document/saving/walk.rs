//! The writer's walk over the snapshot's rows (task 2.4c): which rows are
//! copied, deleted, inserted or written again, and each row's splices.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use super::{Extent, QUOTE_SCAN_ROWS, ROWS_PER_CHECKPOINT, SaveShared, Sink, Streamed, to_i64};
use crate::diagnostics::CountsBuilder;
use crate::dialect::{Bom, LineEnding};
use crate::document::columns::Counts;
use crate::document::view::{RowView, ViewCell};
use crate::document::{Document, Reading, RowBytes, inserted_row, to_usize};
use crate::edit::{CellId, Overlay, Segment};
use crate::rows::{FieldSpan, ParsedRow};
use crate::save::{
    ColumnQuoting, EditedRow, NewCell, RowRules, SaveError, Splice, Transcoder, WholeRow,
    checked_len, row_splices, whole_row_bytes,
};

/// Writes the save to `sink`: the snapshot's logical rows in order, walking
/// its piece list (task 2.4c), with the save oracle's splices, splice for
/// splice (DESIGN §3.7, `docs/tasks/2.4.md` §6):
///
/// - a stretch of original rows is copied in bulk, but for its edited rows
///   and any row whose neighbours changed (its first, and its last if it
///   now ends the file or used to), which go through [`row_splices`];
/// - each deleted original row is one delete of its whole extent;
/// - a run of inserted rows is one insert at the start of the next
///   original row, deleted or not, or at the end ([`whole_row_bytes`]);
/// - with a column operation, every row is looked at, and one whose cells
///   it moved, took or added to is written whole from how it reads now.
///
/// A checkpoint every [`ROWS_PER_CHECKPOINT`] rows written one by one (and
/// the sink's own, between chunks). Refuses [`SaveError::TooLarge`] as soon
/// as the output passes [`MAX_FILE_BYTES`], before the file goes anywhere.
pub(super) fn stream(
    reading: &Reading,
    overlay: &Overlay,
    extent: &Extent<'_>,
    sink: &mut dyn Sink,
    checkpoint: &dyn Fn() -> Result<(), SaveError>,
    progress: Option<&SaveShared>,
) -> Result<Streamed, SaveError> {
    let map = overlay.map();
    let rows = map.rows_within(extent.rows);
    let mut walk = Walk::new(reading, overlay, extent, sink, checkpoint, rows)?;
    walk.progress = progress;
    let mut next: u32 = 0;
    for segment in map.segments(0..rows) {
        match segment {
            Segment::Inserted(range) => walk.inserted(range)?,
            Segment::Original(range) => {
                walk.flush(next)?;
                walk.deleted(next..range.start)?;
                walk.originals(to_usize(range.start)..to_usize(range.end))?;
                next = range.end;
            }
        }
    }
    walk.flush(next)?;
    walk.deleted(next..u32::try_from(extent.rows).unwrap_or(u32::MAX))?;
    walk.finish()
}

/// The line ending the output row before is known to have, or the
/// original row it is (copied as it is).
#[derive(Clone, Copy, Debug)]
enum Prev {
    None,
    Ending(Option<LineEnding>),
    Row(usize),
}

/// [`stream`]'s walk: where it is in the snapshot and in the output.
struct Walk<'w, 's> {
    reading: &'w Reading,
    overlay: &'w Overlay,
    extent: &'w Extent<'w>,
    sink: &'s mut dyn Sink,
    checkpoint: &'w dyn Fn() -> Result<(), SaveError>,
    /// The save's progress, told of the census's pass.
    progress: Option<&'w SaveShared>,
    /// The logical rows written.
    rows: usize,
    /// The output rows written so far.
    out_row: usize,
    /// The snapshot is written (or skipped) up to here.
    at: usize,
    /// How much longer the output is than the snapshot so far.
    delta: i64,
    prev: Prev,
    /// Inserted rows not placed yet, and where each starts in them.
    pending: Vec<u8>,
    pending_starts: Vec<usize>,
    /// Each output row's start, for the new index (not when converting).
    starts: Option<Vec<u32>>,
    /// Each output row's field count, for the new reading (with `starts`),
    /// while the old file's are known.
    counts: Option<CountsBuilder>,
    /// The old file's unterminated quote.
    quote: Option<usize>,
    /// Whether the file quotes every field, once it is needed.
    quote_all: Option<bool>,
    /// Which columns quote new fields, once a new field is written: the
    /// census answers `quote_all` too.
    quoting: Option<ColumnQuoting>,
    /// The file's most common line ending: an inserted row's (ADR-0004
    /// decision 3).
    common: LineEnding,
    /// The file's last row ends with a line ending (decision 4).
    trailing_newline: bool,
    /// Rows written one by one, for the checkpoints.
    handled: usize,
    streamed: Streamed,
}

impl<'w, 's> Walk<'w, 's> {
    fn new(
        reading: &'w Reading,
        overlay: &'w Overlay,
        extent: &'w Extent<'w>,
        sink: &'s mut dyn Sink,
        checkpoint: &'w dyn Fn() -> Result<(), SaveError>,
        rows: usize,
    ) -> Result<Self, SaveError> {
        let quote = extent
            .index
            .unterminated_quote()
            .filter(|_| extent.complete);
        let mut walk = Walk {
            reading,
            overlay,
            extent,
            sink,
            checkpoint,
            progress: None,
            rows,
            out_row: 0,
            at: 0,
            delta: 0,
            prev: Prev::None,
            pending: Vec::new(),
            pending_starts: Vec::new(),
            starts: (!extent.converts()).then(|| Vec::with_capacity(rows)),
            counts: (!extent.converts()).then(|| CountsBuilder::with_capacity(rows)),
            quote,
            quote_all: None,
            quoting: None,
            common: reading.report().line_ending().unwrap_or(LineEnding::Lf),
            trailing_newline: false,
            handled: 0,
            streamed: Streamed {
                unterminated: quote,
                ..Streamed::default()
            },
        };
        walk.trailing_newline = match extent.rows.checked_sub(1) {
            Some(last) => walk.line_ending_of(last)?.is_some(),
            None => false,
        };
        // Save As UTF-8 writes a UTF-8 BOM for the file's BOM (ADR-0008
        // decision 7).
        let bom = reading.detection.bom.len();
        if extent.converts() && bom > 0 && extent.end >= bom {
            walk.sink.splice(Splice {
                range: 0..bom,
                bytes: Bom::Utf8.bytes().to_vec(),
            })?;
            walk.at = bom;
        }
        Ok(walk)
    }

    /// A checkpoint every [`ROWS_PER_CHECKPOINT`] rows.
    fn tick(&mut self) -> Result<(), SaveError> {
        if self.handled.is_multiple_of(ROWS_PER_CHECKPOINT) {
            (self.checkpoint)()?;
        }
        self.handled += 1;
        Ok(())
    }

    /// The rules for a row, with whether the file quotes every field if
    /// `needed`: it reads every row, so only then, once.
    fn rules(&mut self, needed: bool) -> Result<RowRules, SaveError> {
        let quote_all = match (needed, self.quote_all) {
            (false, _) => false,
            (true, Some(known)) => known,
            (true, None) => {
                let known = quotes_every_field(self.reading, self.extent, self.checkpoint)?;
                self.quote_all = Some(known);
                known
            }
        };
        Ok(RowRules {
            encoding: self.extent.target,
            source: self.extent.source,
            delimiter: self.reading.detection.delimiter.byte(),
            quote_all,
        })
    }

    /// Makes the census of which columns quote every field (ADR-0014
    /// decision 4) the first time a row writes a new field: it reads every
    /// row, so only then, once. It finds out whether the file quotes every
    /// field on the way.
    fn census(&mut self) -> Result<(), SaveError> {
        if self.quoting.is_none() {
            let (reading, overlay, checkpoint) = (self.reading, self.overlay, self.checkpoint);
            let pass =
                |read: &dyn Fn(usize)| ColumnQuoting::census(reading, overlay, checkpoint, read);
            let census = match self.progress {
                Some(progress) => progress.checking(self.extent.end, pass),
                None => pass(&|_| {}),
            }?;
            self.quote_all = Some(census.every_field());
            self.quoting = Some(census);
        }
        Ok(())
    }

    /// Where original row `row` starts in the snapshot (the end of what is
    /// written, past the last row).
    fn row_start(&self, row: u32) -> Result<usize, SaveError> {
        let row = to_usize(row);
        if row >= self.extent.rows {
            return Ok(self.extent.end);
        }
        self.extent
            .index
            .row_extent(row)
            .map(|extent| extent.start)
            .ok_or_else(|| SaveError::Failed(format!("row {row} isn't indexed")))
    }

    /// Original row `row`'s bytes, parsed, and its line ending.
    fn read(
        &self,
        row: usize,
    ) -> Result<(RowBytes<'w, 'w>, ParsedRow, Option<LineEnding>), SaveError> {
        // Every row before `rows` can be read: an edit is never dropped.
        let unreadable = || SaveError::Failed(format!("row {row} can't be read"));
        let bytes = Document::row_bytes(self.reading, row)?.ok_or_else(unreadable)?;
        let parsed = self
            .reading
            .parser
            .parse_row_in(bytes.index, row, &bytes.bytes, bytes.base)
            .ok_or_else(unreadable)?;
        let line_ending = bytes
            .index
            .row_in(row, &bytes.bytes, bytes.base)
            .and_then(|span| span.line_ending);
        Ok((bytes, parsed, line_ending))
    }

    fn line_ending_of(&self, row: usize) -> Result<Option<LineEnding>, SaveError> {
        Ok(self.read(row)?.2)
    }

    /// The line ending of the output row before this one.
    fn prev_ending(&self) -> Result<Option<LineEnding>, SaveError> {
        match self.prev {
            Prev::None => Ok(None),
            Prev::Ending(ending) => Ok(ending),
            Prev::Row(row) => self.line_ending_of(row),
        }
    }

    /// The line ending output row `self.out_row` gets, if its own is
    /// `own` (`None` for an inserted row): none if it ends a file with no
    /// final newline, otherwise its own, or the most common one.
    fn ending(&self, own: Option<LineEnding>) -> Option<LineEnding> {
        if self.out_row + 1 == self.rows && !self.trailing_newline {
            None
        } else {
            own.or(Some(self.common))
        }
    }

    /// Notes that the output row about to be written starts at snapshot
    /// offset `offset`, moved by the splices so far.
    fn push_start(&mut self, offset: usize) {
        if let Some(starts) = &mut self.starts {
            match u32::try_from(to_i64(offset).saturating_add(self.delta)) {
                Ok(start) => starts.push(start),
                // Past 4 GiB: the save is refused as too large.
                Err(_) => self.starts = None,
            }
        }
    }

    /// Notes the field count of the output row being written: `None` for a
    /// blank line.
    fn push_count(&mut self, fields: Option<usize>) {
        if let Some(counts) = &mut self.counts {
            counts.push(fields);
        }
    }

    /// Notes the field count of a row written whole as `out` with line
    /// ending `ending`, from `cells`: one field each (a field's bytes
    /// copied hold no delimiter outside quotes), or one (`""`) for none,
    /// or a blank line if it has no bytes.
    fn push_whole_count(&mut self, out: &[u8], ending: Option<LineEnding>, cells: usize) {
        let content = out.len() - ending.map_or(0, |ending| ending.bytes().len());
        self.push_count((content > 0).then_some(cells.max(1)));
    }

    /// Writes the snapshot up to `splice`, then `splice`. `drops_quote`:
    /// if it covers the old file's unterminated quote, the quote is gone
    /// (its row deleted, or its field edited, which closes it). Otherwise
    /// the quote's bytes are kept (also at the end of a splice of the
    /// whole row, a fix), so it moves by each splice's change in length up
    /// to it.
    fn put(&mut self, splice: Splice, drops_quote: bool) -> Result<(), SaveError> {
        if splice.range.start < self.at || splice.range.end < splice.range.start {
            return Err(SaveError::Failed(format!(
                "splice {:?} out of order after {}",
                splice.range, self.at
            )));
        }
        let change = to_i64(splice.bytes.len()) - to_i64(splice.range.len());
        if let Some(q) = self.quote
            && let Some(moved) = self.streamed.unterminated
            && splice.range.start <= q
        {
            let over_it = splice.range.end > q || splice.range.start == q;
            self.streamed.unterminated = if drops_quote && over_it {
                None
            } else {
                usize::try_from(to_i64(moved).saturating_add(change)).ok()
            };
        }
        self.sink.copy(self.at..splice.range.start)?;
        self.at = splice.range.end;
        self.delta += change;
        self.sink.splice(splice)?;
        if !self.extent.converts() {
            // Converting, the sink checks the length of what it writes:
            // the bytes between splices change length too.
            checked_len(self.at, self.delta).map_err(|len| SaveError::TooLarge { len })?;
        }
        Ok(())
    }

    /// Inserted rows `range` (by number), made ready to be placed before
    /// the next original row.
    fn inserted(&mut self, range: Range<u32>) -> Result<(), SaveError> {
        let first_without_bom = self.reading.detection.bom.is_empty();
        for n in range {
            self.tick()?;
            let Some((row, edits)) = inserted_row(self.overlay, n) else {
                return Err(SaveError::Failed(format!("inserted row {n} is missing")));
            };
            let view = RowView::inserted(&self.reading.parser, row, edits);
            // Its own values and a column insert's are new fields; an
            // edited cell past them is a hatched one.
            let cells: Vec<NewCell<'_>> = (0..view.len())
                .map(|column| match (view.cell_id(column), view.cell(column)) {
                    (Some(CellId::Appended(_)), Some(ViewCell::Edited(value))) => {
                        NewCell::Hatched(value)
                    }
                    (_, Some(ViewCell::New(value) | ViewCell::Edited(value))) => {
                        NewCell::New(value)
                    }
                    _ => NewCell::Padding,
                })
                .collect();
            if cells.iter().any(|cell| matches!(cell, NewCell::New(_))) {
                self.census()?;
            }
            let rules = self.rules(true)?;
            let ending = self.ending(None);
            let whole = WholeRow {
                row: self.out_row,
                cells: &cells,
                ending,
                blank_line: false,
                after_cr: false,
                first_without_bom: first_without_bom && self.out_row == 0,
            };
            let no_census = ColumnQuoting::default();
            let quoting = self.quoting.as_ref().unwrap_or(&no_census);
            let (bytes, ending) = whole_row_bytes(&whole, rules, quoting, &mut self.streamed.fixes)
                .map_err(|columns| {
                    // `check_encodable` passed every value.
                    SaveError::Failed(format!(
                        "inserted row {}'s columns {columns:?} can't be encoded",
                        self.out_row
                    ))
                })?;
            self.push_whole_count(&bytes, ending, cells.len());
            self.pending_starts.push(self.pending.len());
            self.pending.extend_from_slice(&bytes);
            self.out_row += 1;
            self.prev = Prev::Ending(ending);
        }
        Ok(())
    }

    /// Places the inserted rows waiting, if any: one insert at the start
    /// of original row `next` (or at the end).
    fn flush(&mut self, next: u32) -> Result<(), SaveError> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let offset = self.row_start(next)?;
        for at in std::mem::take(&mut self.pending_starts) {
            self.push_start(offset + at);
        }
        let bytes = std::mem::take(&mut self.pending);
        self.put(
            Splice {
                range: offset..offset,
                bytes,
            },
            false,
        )
    }

    /// Deleted original rows `rows`: one delete of each, line ending
    /// included.
    fn deleted(&mut self, rows: Range<u32>) -> Result<(), SaveError> {
        for row in rows {
            self.tick()?;
            let range = self
                .extent
                .index
                .row_extent(to_usize(row))
                .ok_or_else(|| SaveError::Failed(format!("deleted row {row} isn't indexed")))?;
            self.put(
                Splice {
                    range,
                    bytes: Vec::new(),
                },
                true,
            )?;
        }
        Ok(())
    }

    /// Original rows `rows`, a stretch the piece list keeps together:
    /// copied, but for the rows that need a look of their own.
    fn originals(&mut self, rows: Range<usize>) -> Result<(), SaveError> {
        if rows.is_empty() {
            return Ok(());
        }
        if !self.overlay.columns().is_empty() {
            // A column operation may reach any row: each is looked at.
            for row in rows {
                self.original(row)?;
            }
            return Ok(());
        }
        let last = rows.end - 1;
        // Its first row may now follow other rows than it did (a lone CR,
        // decision 10) or start the file (a BOM-like field); its last may
        // now end the file, or no longer (decision 4).
        let mut look: Vec<usize> = self
            .overlay
            .rows_in(rows.clone())
            .map(|(row, _)| row)
            .collect();
        look.push(rows.start);
        if self.out_row + rows.len() == self.rows || last + 1 == self.extent.rows {
            look.push(last);
        }
        look.sort_unstable();
        look.dedup();
        let mut looks = look.into_iter().peekable();
        let mut from = rows.start;
        // A row after one whose line ending became CR (a blank line split
        // from a CR before it), which may need splitting too.
        let mut after_split = None;
        loop {
            let next = match (looks.peek().copied(), after_split) {
                (Some(row), Some(split)) => row.min(split),
                (Some(row), None) => row,
                (None, Some(split)) => split,
                (None, None) => break,
            };
            if looks.peek() == Some(&next) {
                looks.next();
            }
            if after_split == Some(next) {
                after_split = None;
            }
            self.copied(from..next);
            if self.original(next)? && next < last {
                after_split = Some(next + 1);
            }
            from = next + 1;
        }
        self.copied(from..rows.end);
        Ok(())
    }

    /// Original rows `rows`, copied as they are (the copy is made with the
    /// next splice, or at the end).
    fn copied(&mut self, rows: Range<usize>) {
        if rows.is_empty() {
            return;
        }
        if let Some(starts) = &mut self.starts
            && !self
                .extent
                .index
                .extend_starts(rows.clone(), self.delta, starts)
        {
            self.starts = None;
        }
        // Copied as they are: the old file's counts.
        if let Some(counts) = &mut self.counts
            && !Counts::of(self.reading).is_some_and(|old| old.copy_counts(rows.clone(), counts))
        {
            self.counts = None;
        }
        self.out_row += rows.len();
        self.prev = Prev::Row(rows.end - 1);
    }

    /// Original row `row`, written with its edits and any change its new
    /// neighbours bring. Returns whether its line ending became a lone CR.
    fn original(&mut self, row: usize) -> Result<bool, SaveError> {
        self.tick()?;
        let (bytes, parsed, line_ending) = self.read(row)?;
        let overlay = self.overlay;
        let ending = self.ending(line_ending);
        let view = RowView::new(
            &self.reading.parser,
            &bytes.bytes,
            bytes.base,
            &parsed,
            overlay.physical(row),
        );
        // Its edited cells shown, by column: not a deleted column's.
        let cells: Vec<(usize, Arc<str>)> = overlay.row(row).map_or_else(Vec::new, |edits| {
            edits
                .shown(overlay.columns(), None)
                .into_iter()
                .map(|(column, value)| (column, Arc::clone(value)))
                .collect()
        });
        let fields = parsed.fields().len();
        let len = fields.max(cells.last().map_or(0, |&(column, _)| column + 1));
        if !view.same_shape() || view.len() != len {
            // Cells moved, taken or added by a column operation.
            return self.whole(row, &view, &bytes, &parsed, line_ending, ending);
        }
        let blank = parsed.span().is_empty();
        let after_cr = blank
            && cells.is_empty()
            && ending == Some(LineEnding::Lf)
            && self.prev_ending()? == Some(LineEnding::Cr);
        // Whether the file quotes every field matters only to an edited
        // cell with no quoting of its own: a hatched cell, or a blank
        // line's one field (a file that quotes every field quotes every
        // other one already).
        let unquoted = !cells.is_empty() && (len > fields || blank);
        let rules = self.rules(unquoted)?;
        let edited = EditedRow {
            row: self.out_row,
            bytes: &bytes.bytes,
            base: bytes.base,
            span: parsed.span(),
            line_ending,
            ending,
            after_cr,
            fields: parsed.fields(),
            cells: &cells,
            first_without_bom: self.out_row == 0 && self.reading.detection.bom.is_empty(),
        };
        // The old file's open quote, if it is in this row: its field
        // edited, the quote closes (the value is written quoted).
        let open_field_edited = self.quote.is_some_and(|q| {
            parsed
                .fields()
                .iter()
                .position(|field| field.start() == q)
                .is_some_and(|column| cells.binary_search_by_key(&column, |&(c, _)| c).is_ok())
        });
        self.push_start(bytes.base);
        let mut splices = Vec::new();
        let written = match row_splices(&edited, rules, &mut splices, &mut self.streamed.fixes) {
            Ok(written) => written,
            Err(columns) => {
                if !self.extent.converts() {
                    // `check_encodable` passed every edited value.
                    return Err(SaveError::Failed(format!(
                        "row {row}'s columns {columns:?} can't be encoded"
                    )));
                }
                // Save As UTF-8: a fix that rewrites the row met an
                // unedited field that can't be converted. The save will
                // stop naming it and any other such field of the row; the
                // rest of the file is still read, for the rest of the
                // cells.
                let mut unconvertible = edited.unconvertible(rules);
                if unconvertible.is_empty() {
                    unconvertible = columns;
                }
                let named: Vec<(usize, usize)> =
                    unconvertible.iter().map(|&c| (self.out_row, c)).collect();
                self.sink.copy(self.at..edited.span.start)?;
                self.sink.refuse(&named)?;
                self.at = edited.end(rules);
                self.out_row += 1;
                self.prev = Prev::Ending(ending);
                return Ok(false);
            }
        };
        for splice in splices {
            debug_assert!(
                bytes.index.row_extent(row).is_some_and(|extent| {
                    extent.start <= splice.range.start && splice.range.end <= extent.end
                }),
                "splice {:?} outside row {row}",
                splice.range
            );
            self.put(splice, open_field_edited)?;
        }
        // Its own fields and hatched cells, in place; with no bytes, a
        // blank line stays one while it has a line ending (`row_splices`
        // writes any other row with no bytes `""`).
        let blank_out = blank && cells.is_empty() && ending.is_some();
        self.push_count((!blank_out).then_some(len));
        self.out_row += 1;
        self.prev = Prev::Ending(written);
        Ok(written == Some(LineEnding::Cr) && line_ending != Some(LineEnding::Cr))
    }

    /// Original row `row`, read as `view`, whose cells a column operation
    /// moved, took or added to: written whole from how it reads now, as the
    /// oracle does, unless that is what the file has. Returns whether its
    /// line ending became a lone CR.
    fn whole(
        &mut self,
        row: usize,
        view: &RowView<'_>,
        bytes: &RowBytes<'_, '_>,
        parsed: &ParsedRow,
        line_ending: Option<LineEnding>,
        ending: Option<LineEnding>,
    ) -> Result<bool, SaveError> {
        let base = bytes.base;
        let range = self
            .extent
            .index
            .row_extent(row)
            .ok_or_else(|| SaveError::Failed(format!("row {row} isn't indexed")))?;
        let raw = |field: &FieldSpan| {
            let span = field.span();
            &bytes.bytes[span.start - base..span.end - base]
        };
        let converts = self.extent.converts();
        let source = self.extent.source;
        let mut cells: Vec<NewCell<'_>> = Vec::with_capacity(view.len());
        let mut unconvertible = Vec::new();
        // The old file's open quote's field, if it is still there unedited:
        // its length as written. Nothing can follow it (2.4b's
        // `AfterUnterminatedQuote`), so it ends the row's content.
        let mut quote_len = None;
        for column in 0..view.len() {
            let cell = match (view.cell_id(column), view.cell(column)) {
                (_, Some(ViewCell::Field(field))) => {
                    let bytes = if converts {
                        Transcoder::convert(raw(field), source)
                    } else {
                        Some(Cow::Borrowed(raw(field)))
                    };
                    if self.quote == Some(field.start()) {
                        debug_assert_eq!(column + 1, view.len(), "a cell after the open quote");
                        quote_len = bytes.as_ref().map(|bytes| bytes.len());
                    }
                    bytes.map_or_else(
                        || {
                            unconvertible.push(column);
                            NewCell::Padding
                        },
                        NewCell::Bytes,
                    )
                }
                (Some(CellId::Field(k)), Some(ViewCell::Edited(value))) => NewCell::Edited {
                    value,
                    quoted: usize::try_from(k)
                        .ok()
                        .and_then(|k| parsed.field(k))
                        .is_some_and(FieldSpan::quoted),
                },
                (Some(CellId::Appended(_)), Some(ViewCell::Edited(value))) => {
                    NewCell::Hatched(value)
                }
                (_, Some(ViewCell::New(value) | ViewCell::Edited(value))) => NewCell::New(value),
                _ => NewCell::Padding,
            };
            cells.push(cell);
        }
        if !unconvertible.is_empty() {
            // Save As UTF-8: the save will stop naming these cells; the
            // rest of the file is still read, for the rest of the cells.
            let named: Vec<(usize, usize)> =
                unconvertible.iter().map(|&c| (self.out_row, c)).collect();
            self.sink.copy(self.at..range.start)?;
            self.sink.refuse(&named)?;
            self.at = range.end;
            self.out_row += 1;
            self.prev = Prev::Ending(ending);
            return Ok(false);
        }
        if cells.iter().any(|cell| matches!(cell, NewCell::New(_))) {
            self.census()?;
        }
        // Whether the file quotes every field matters only to a value.
        let values = cells
            .iter()
            .any(|cell| !matches!(cell, NewCell::Bytes(_) | NewCell::Padding));
        let rules = self.rules(values)?;
        let blank_line = parsed.span().is_empty();
        let after_cr = blank_line
            && ending == Some(LineEnding::Lf)
            && self.prev_ending()? == Some(LineEnding::Cr);
        let whole = WholeRow {
            row: self.out_row,
            cells: &cells,
            ending,
            blank_line,
            after_cr,
            first_without_bom: self.out_row == 0 && self.reading.detection.bom.is_empty(),
        };
        let no_census = ColumnQuoting::default();
        let quoting = self.quoting.as_ref().unwrap_or(&no_census);
        let (out, written) = whole_row_bytes(&whole, rules, quoting, &mut self.streamed.fixes)
            .map_err(|columns| {
                // `check_encodable` passed every value.
                SaveError::Failed(format!("row {row}'s columns {columns:?} can't be encoded"))
            })?;
        self.push_whole_count(&out, written, cells.len());
        self.push_start(range.start);
        if out != bytes.bytes[range.start - base..range.end - base] {
            let start = to_i64(range.start).saturating_add(self.delta);
            let content = out.len() - written.map_or(0, |ending| ending.bytes().len());
            let moved = self.quote.filter(|&q| range.contains(&q)).map(|_| {
                quote_len.and_then(|len| usize::try_from(start + to_i64(content - len)).ok())
            });
            self.put(Splice { range, bytes: out }, false)?;
            if let Some(moved) = moved {
                self.streamed.unterminated = moved;
            }
        }
        self.out_row += 1;
        self.prev = Prev::Ending(written);
        Ok(written == Some(LineEnding::Cr) && line_ending != Some(LineEnding::Cr))
    }

    /// The rest of the snapshot, and what the walk found.
    fn finish(mut self) -> Result<Streamed, SaveError> {
        debug_assert!(self.pending.is_empty(), "inserted rows left over");
        debug_assert_eq!(self.out_row, self.rows, "rows written");
        self.sink.copy(self.at..self.extent.end)?;
        // In the order the oracle makes them (ADR-0004 decisions 6, 10,
        // then 7): the rows written `""`, the CR splits, then the BOM-like
        // first field.
        self.streamed.fixes.sort_by_key(|fix| fix.rank());
        self.streamed.len =
            checked_len(self.extent.end, self.delta).map_err(|len| SaveError::TooLarge { len })?;
        self.streamed.rows = self.rows;
        self.streamed.starts = self.starts;
        let counts = self.counts.filter(|counts| counts.len() == self.rows);
        self.streamed.mode = match &counts {
            Some(counts) => counts.mode(),
            None if self.rows == 0 => None,
            None => {
                let columns = self.overlay.columns();
                let mode = self.extent.index.field_count_mode();
                mode.map(|mode| columns.fold_fields(mode))
            }
        };
        self.streamed.counts = counts.map(CountsBuilder::finish);
        Ok(self.streamed)
    }
}

/// Whether every field of every non-blank row of `extent` is quoted
/// (ADR-0004 decision 1). It reads the rows a batch at a time, and stops at
/// the first field that isn't, which in most files is the first.
fn quotes_every_field(
    reading: &Reading,
    extent: &Extent<'_>,
    checkpoint: &dyn Fn() -> Result<(), SaveError>,
) -> Result<bool, SaveError> {
    let stale = reading.head_is_stale();
    let index = extent.index;
    let parser = &reading.parser;
    let mut any = false;
    let mut start = 0;
    while start < extent.rows {
        checkpoint()?;
        let batch = start..extent.rows.min(start + QUOTE_SCAN_ROWS);
        let Some(range) = index.rows_extent(batch.clone()) else {
            break;
        };
        let bytes = reading.bytes_of(range.clone(), stale)?;
        for row in batch.clone() {
            let Some(parsed) = parser.parse_row_in(index, row, &bytes, range.start) else {
                continue;
            };
            if parsed.span().is_empty() {
                continue; // a blank line
            }
            any = true;
            if !parsed.fields().iter().all(|field| field.quoted()) {
                return Ok(false);
            }
        }
        start = batch.end;
    }
    Ok(any)
}
