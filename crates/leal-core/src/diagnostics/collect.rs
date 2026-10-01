//! Collecting diagnostics during the index's pass.
//!
//! The [`Collector`] is the index scanner's [`RowObserver`]. The scanner
//! tells it about every row as it ends (for ragged rows, blank lines and
//! line endings), every closing quote with text after it, the unterminated
//! quote, and every stretch of units it passes, saying which field of which
//! row each stretch belongs to.
//!
//! **Field-level kinds** (invalid encoding and NUL) need to know which field
//! a byte is in, but the scanner never splits rows into fields: it only
//! counts delimiters. So the collector finds the bytes themselves with
//! fast whole-chunk searches ([`Finder`]): `memchr` for 0x00, SIMD UTF-8
//! validation for invalid UTF-8. In a clean file these find nothing, and
//! each stretch the scanner reports costs one comparison. Only when a stretch
//! holds a hit does the collector work out its field, by counting the
//! delimiters between the stretch's start and the hit. After a hit, the
//! search skips to the next delimiter, CR or LF, since nothing before it
//! can be in another field, so a field full of NULs costs no more than
//! one with a single NUL.

use std::sync::Arc;

use super::{Diagnostic, DiagnosticKind, Diagnostics, Location, MAX_LOCATIONS, Report, marks};
use crate::dialect::Encoding;
use crate::index::scan::{FieldCounts, RowFacts, RowObserver, Segment, Units, View};
use crate::index::{CodeUnit, LineEnding};

const CR: u8 = b'\r';
const LF: u8 = b'\n';

/// No hit is known: the search has reached its window's end.
const NONE: usize = usize::MAX;

/// While every row so far is among the first this many non-blank rows,
/// every row is kept as a possible ragged row; see [`Collector::row`].
const KEEP_ALL_ROWS: u64 = 2 * MAX_LOCATIONS as u64;

// ---------------------------------------------------------------------------
// Bounded tallies

/// The count of one kind and its first [`MAX_LOCATIONS`] locations.
///
/// A [`Report`] covers whole rows only, so occurrences in a row that hasn't
/// ended yet are left out of it. Field-level kinds are found while their
/// row is being scanned, so their tally remembers how many occurrences are
/// in the last row it was given one for ([`Tally::push_in_row`]). That
/// needs no work at each row's end, which matters in a file of millions of
/// short rows. Row-level kinds are counted when their row ends
/// ([`Tally::push`]).
#[derive(Debug, Default)]
struct Tally {
    count: usize,
    first: Vec<Location>,
    /// For [`Tally::push_in_row`]: the row of the last occurrence, and how
    /// many occurrences it has.
    last_row: usize,
    in_last_row: usize,
}

impl Tally {
    /// Adds an occurrence in a row that has ended. Occurrences come in file
    /// order.
    fn push(&mut self, location: Location) {
        if self.first.len() < MAX_LOCATIONS {
            self.first.push(location);
        }
        self.count += 1;
    }

    /// Adds an occurrence in the row being scanned.
    fn push_in_row(&mut self, location: Location) {
        if self.count == 0 || location.row != self.last_row {
            self.last_row = location.row;
            self.in_last_row = 0;
        }
        self.in_last_row += 1;
        self.push(location);
    }

    /// The diagnostic for the occurrences in the first `rows` rows, if
    /// there are any.
    fn diagnostic(&self, kind: DiagnosticKind, rows: usize) -> Option<Diagnostic> {
        let count = if self.count > 0 && self.last_row >= rows {
            self.count - self.in_last_row
        } else {
            self.count
        };
        (count > 0).then(|| Diagnostic {
            kind,
            count,
            // `first` holds the first occurrences in file order, so those in
            // finished rows are at its start.
            first: self.first[..count.min(self.first.len())].to_vec(),
        })
    }
}

// ---------------------------------------------------------------------------
// Finding bytes inside fields

/// What a [`Finder`] looks for.
#[derive(Debug)]
enum Target {
    /// A 0x00 byte.
    NulByte,
    /// A U+0000 code unit: two 0x00 bytes at a unit's start.
    NulUnit,
    /// The start of an invalid UTF-8 sequence.
    InvalidUtf8,
    /// An unpaired UTF-16 surrogate, or a final odd byte.
    InvalidUtf16 { big_endian: bool },
    /// A byte that a single-byte encoding doesn't map (it displays as
    /// U+FFFD). Indexed by byte value.
    Unmapped(Box<[bool; 256]>),
}

impl Target {
    /// How many bytes past a chunk's end a search must read to decide
    /// whether a sequence that starts before the end is valid: the rest of
    /// a UTF-8 sequence, or the low surrogate after a high one.
    fn lookahead(&self) -> usize {
        match self {
            Target::InvalidUtf8 => 3,
            Target::InvalidUtf16 { .. } => 2,
            Target::NulByte | Target::NulUnit | Target::Unmapped(_) => 0,
        }
    }
}

/// The result of one search.
#[derive(Debug, PartialEq, Eq)]
enum Search {
    /// A hit at this offset.
    Hit(usize),
    /// No hit before this offset. It can stop short of the window's end,
    /// at a sequence that the window cuts off; the next search starts
    /// there.
    Clear(usize),
}

/// Finds one kind of byte in a field, in file order, a chunk's window at a
/// time, and keeps the first one in each field.
#[derive(Debug)]
struct Finder {
    target: Target,
    /// The next hit, or [`NONE`] if there's none before `frontier`.
    next: usize,
    /// Where the next search starts, when `next` is [`NONE`].
    frontier: usize,
    /// The end of the current window: the chunk's end plus the target's
    /// lookahead.
    limit: usize,
    /// The file's length.
    len: usize,
    /// The field of the last hit counted, as `(row, field)`.
    last_field: Option<(usize, usize)>,
    tally: Tally,
}

impl Finder {
    /// A finder that starts at `start`, the end of the BOM: where row 0,
    /// and in UTF-16 the first code unit, starts.
    fn new(target: Target, start: usize) -> Self {
        Finder {
            target,
            next: NONE,
            frontier: start,
            limit: start,
            len: 0,
            last_field: None,
            tally: Tally::default(),
        }
    }

    /// Widens the window to cover the chunk that ends at `to`, of a
    /// `len`-byte file. `view` holds the file's bytes at least up to the
    /// target's lookahead past `to` (`scan::LOOKAHEAD`), or to the end.
    fn extend<U: Units>(&mut self, view: &View<'_, U>, to: usize, len: usize) {
        self.len = len;
        let limit = to.saturating_add(self.target.lookahead()).min(len);
        debug_assert!(limit <= view.end(), "the view ends before the lookahead");
        self.limit = self.limit.max(limit.min(view.end()));
        if self.next == NONE && self.frontier < self.limit {
            self.search_from(view, self.frontier);
        }
    }

    fn search_from<U: Units>(&mut self, view: &View<'_, U>, from: usize) {
        let window = view.slice(from, self.limit);
        let at_eof = self.limit == self.len;
        match self.search(window, at_eof) {
            Search::Hit(at) => self.next = from + at,
            Search::Clear(upto) => {
                self.next = NONE;
                self.frontier = from + upto;
            }
        }
    }

    /// The first hit in `window`, as an offset into it. The window starts
    /// at the start of a code unit, and in UTF-8 at the start of a sequence
    /// (an ASCII byte always is). `at_eof` if it ends at the end of the
    /// file.
    fn search(&self, window: &[u8], at_eof: bool) -> Search {
        let end = window.len();
        match &self.target {
            Target::NulByte => memchr::memchr(0, window).map_or(Search::Clear(end), Search::Hit),
            Target::NulUnit => {
                let at = skip_clean_blocks(window, 0, end, |unit| unit == [0, 0]);
                // Whole units only: `as_chunks` leaves a final odd byte out.
                let (units, _odd) = window[at..].as_chunks::<2>();
                units
                    .iter()
                    .position(|&unit| unit == [0, 0])
                    .map_or(Search::Clear(end), |i| Search::Hit(at + 2 * i))
            }
            Target::InvalidUtf8 => {
                // Where invalid text is dense, the next hit is a few bytes
                // away. Setting up the SIMD validator over the whole window
                // costs more than that, so the first `NEAR` bytes are
                // checked with the standard library's validator, which is
                // quick on short slices.
                let near = NEAR.min(end);
                let first = std::str::from_utf8(&window[..near])
                    .err()
                    .map(|e| (e.valid_up_to(), e.error_len().is_none()));
                match utf8_search(first, 0, near, at_eof && near == end) {
                    Search::Clear(upto) => {
                        let rest = simdutf8::compat::from_utf8(&window[upto..])
                            .err()
                            .map(|e| (e.valid_up_to(), e.error_len().is_none()));
                        utf8_search(rest, upto, end, at_eof)
                    }
                    hit => hit,
                }
            }
            Target::InvalidUtf16 { big_endian } => {
                utf16_invalid(window, 0, end, *big_endian, at_eof)
            }
            Target::Unmapped(unmapped) => window
                .iter()
                .position(|&b| unmapped[usize::from(b)])
                .map_or(Search::Clear(end), Search::Hit),
        }
    }

    /// Counts the hits in one stretch of the scan, once per field. True if
    /// there were any.
    fn take<U: Units>(&mut self, view: &View<'_, U>, delimiter: u8, segment: Segment) -> bool {
        let Segment {
            row,
            mut field,
            from,
            to,
            quoted,
        } = segment;
        let any = self.next < to;
        let mut counted_to = from;
        while self.next < to {
            let at = self.next;
            debug_assert!(at >= from, "a hit at {at} before the stretch {from}..{to}");
            if !quoted {
                // Outside quotes, a delimiter starts the next field.
                field += view.count(counted_to, at, delimiter);
                counted_to = at;
            }
            if self.last_field != Some((row, field)) {
                self.last_field = Some((row, field));
                self.tally.push_in_row(Location { row, offset: at });
            }
            // Every field boundary is a delimiter or a line ending, so the
            // next hit in another field is after the next delimiter, CR or
            // LF. A hit before then is in the same field.
            //
            // If there isn't one in the window, the search resumes at the
            // window's end, which can be in the middle of a character. A
            // false hit there is in the same field as this one (there is no
            // boundary in between), so it isn't counted.
            let resume = next_boundary(view, at, self.limit, delimiter);
            self.search_from(view, resume);
        }
        any
    }
}

/// How far a search looks one unit at a time before it hands over to a
/// search that is faster per byte but slower to start: `memchr3` or the
/// SIMD UTF-8 validator. When every field holds a hit, the next delimiter
/// or hit is only a few bytes away, and starting those searches once per
/// field would cost several times the index itself.
const NEAR: usize = 64;

/// The first delimiter, CR or LF in `from..end`, or `end`: where the next
/// field can start.
fn next_boundary<U: Units>(view: &View<'_, U>, from: usize, end: usize, delimiter: u8) -> usize {
    let near = (from + NEAR).min(end);
    let boundary = |at: &usize| view.is(*at, delimiter) || view.is(*at, CR) || view.is(*at, LF);
    (from..near)
        .step_by(U::WIDTH)
        .find(boundary)
        .or_else(|| view.find3(near, end, [delimiter, CR, LF]).map(|(b, _)| b))
        .unwrap_or(end)
}

/// The result of a UTF-8 search of `from..end`, given the validator's
/// error, if any, as `(valid_up_to, cut_off)`: where the first invalid
/// sequence starts, and whether it is a sequence the end of the window cut
/// off (`error_len() == None`), rather than one that is invalid whatever
/// follows.
fn utf8_search(error: Option<(usize, bool)>, from: usize, end: usize, at_eof: bool) -> Search {
    match error {
        None => Search::Clear(end),
        // A sequence cut off by the window's end is invalid only at the end
        // of the file. Otherwise the next window decides.
        Some((valid, true)) if !at_eof => Search::Clear(from + valid),
        Some((valid, _)) => Search::Hit(from + valid),
    }
}

/// The bytes in a block that [`skip_clean_blocks`] checks at once.
const BLOCK: usize = 64;

/// Where to start looking for a UTF-16 code unit that `maybe` accepts in
/// `from..end`: the start of the first whole [`BLOCK`] after `from` with
/// such a unit in it, or of the part too short to be a block. `from` is the
/// start of a unit.
///
/// Checking a block is a fixed-size loop with no early exit, which the
/// compiler turns into vector instructions, so clean text is skipped about
/// 30 times faster than one unit at a time.
fn skip_clean_blocks(
    bytes: &[u8],
    from: usize,
    end: usize,
    maybe: impl Fn([u8; 2]) -> bool,
) -> usize {
    let (blocks, _rest) = bytes[from..end].as_chunks::<BLOCK>();
    let first_dirty = blocks.iter().position(|block| {
        let (units, _) = block.as_chunks::<2>();
        units.iter().fold(false, |any, &unit| any | maybe(unit))
    });
    from + BLOCK * first_dirty.unwrap_or(blocks.len())
}

/// [`Finder::search`] for UTF-16: the first unpaired surrogate in
/// `from..end`, or a final odd byte.
fn utf16_invalid(bytes: &[u8], from: usize, end: usize, big_endian: bool, at_eof: bool) -> Search {
    let unit = |at: usize| {
        let pair = [bytes[at], bytes[at + 1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    };
    let is_low = |u: u16| (0xDC00..=0xDFFF).contains(&u);
    // Every surrogate, high or low, has 0xD8 to 0xDF in its high byte.
    let high_byte = usize::from(!big_endian);
    let surrogate = |unit: [u8; 2]| unit[high_byte] & 0xF8 == 0xD8;
    // Units are checked one at a time from the start of a block that may
    // hold a surrogate, until the end of that block; then clean blocks are
    // skipped again. A pair is never split, because a skipped block holds
    // no surrogates at all.
    let mut at = skip_clean_blocks(bytes, from, end, surrogate);
    let mut recheck = at + BLOCK;
    while at + 2 <= end {
        if at >= recheck {
            at = skip_clean_blocks(bytes, at, end, surrogate);
            recheck = at + BLOCK;
            continue;
        }
        match unit(at) {
            0xD800..=0xDBFF => {
                if at + 4 <= end {
                    if is_low(unit(at + 2)) {
                        at += 4; // a pair
                        continue;
                    }
                    return Search::Hit(at);
                }
                // The window ends before the next unit: it decides only at
                // the end of the file.
                return if at_eof {
                    Search::Hit(at)
                } else {
                    Search::Clear(at)
                };
            }
            u if is_low(u) => return Search::Hit(at),
            _ => at += 2,
        }
    }
    if at < end {
        // A final odd byte, which isn't a whole unit.
        Search::Hit(at)
    } else {
        Search::Clear(end)
    }
}

/// The bytes that `encoding` (a single-byte encoding, not UTF-8) doesn't
/// map, or `None` if it maps every byte.
fn unmapped_bytes(encoding: Encoding) -> Option<Box<[bool; 256]>> {
    // ISO-8859-1 maps every byte to the code point of the same value, so it
    // has no decoder here; the rest use `encoding_rs`'s tables, as display
    // values do (1.4).
    let decoder = encoding.whatwg()?;
    let mut unmapped = Box::new([false; 256]);
    let mut any = false;
    for b in 0..=u8::MAX {
        let byte = [b];
        let (text, _) = decoder.decode_without_bom_handling(&byte);
        if text.contains(char::REPLACEMENT_CHARACTER) {
            unmapped[usize::from(b)] = true;
            any = true;
        }
    }
    any.then_some(unmapped)
}

// ---------------------------------------------------------------------------
// The collector

/// A non-blank row that may turn out to be ragged.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    location: Location,
    fields: usize,
}

/// Collects diagnostics as the scanner reports what it finds, and publishes
/// a [`Report`] to its [`Diagnostics`] after each chunk.
pub(crate) struct Collector {
    diagnostics: Arc<Diagnostics>,
    delimiter: u8,
    nul: Option<Finder>,
    invalid: Option<Finder>,
    /// The nearer of the two finders' next hits.
    next_hit: usize,
    unterminated: Option<Location>,
    text_after_quote: Tally,
    blank_lines: Tally,
    /// One tally per line ending, indexed by [`ending_index`].
    line_endings: [Tally; 3],
    /// The order each line ending was first seen in (for ties), indexed the
    /// same way.
    first_seen: [Option<usize>; 3],
    seen: usize,
    /// Rows that may be ragged; see [`Collector::row`].
    candidates: Vec<Candidate>,
    /// The most common field count so far and how many rows have it.
    mode: Option<(usize, u64)>,
    /// Non-blank rows so far.
    non_blank: u64,
    /// The row being scanned has a field-level warning or the unterminated
    /// quote, so far.
    row_flagged: bool,
    /// Marks of the rows finished since the last publish ([`marks::code`]),
    /// and the exact field counts of the wide ones.
    new_codes: Vec<u8>,
    new_wide: Vec<(u32, u32)>,
}

impl Collector {
    pub(crate) fn new(diagnostics: Arc<Diagnostics>) -> Self {
        let dialect = diagnostics.dialect();
        let base = dialect.bom_len;
        // A NUL delimiter or quote makes 0x00 structure, not text, so it
        // isn't reported. Detection never chooses one (DESIGN §3.2).
        let nul_is_text = dialect.delimiter != 0 && dialect.quote != 0;
        let (nul, invalid) = match dialect.code_unit {
            CodeUnit::Byte => (
                Target::NulByte,
                match diagnostics.encoding() {
                    Encoding::Utf8 => Some(Target::InvalidUtf8),
                    other => unmapped_bytes(other).map(Target::Unmapped),
                },
            ),
            CodeUnit::Utf16Le | CodeUnit::Utf16Be => (
                Target::NulUnit,
                Some(Target::InvalidUtf16 {
                    big_endian: dialect.code_unit == CodeUnit::Utf16Be,
                }),
            ),
        };
        Collector {
            diagnostics,
            delimiter: dialect.delimiter,
            nul: nul_is_text.then(|| Finder::new(nul, base)),
            invalid: invalid.map(|t| Finder::new(t, base)),
            next_hit: NONE,
            unterminated: None,
            text_after_quote: Tally::default(),
            blank_lines: Tally::default(),
            line_endings: Default::default(),
            first_seen: [None; 3],
            seen: 0,
            candidates: Vec::new(),
            mode: None,
            non_blank: 0,
            row_flagged: false,
            new_codes: Vec::new(),
            new_wide: Vec::new(),
        }
    }

    fn finders(&mut self) -> impl Iterator<Item = &mut Finder> {
        self.nul.iter_mut().chain(self.invalid.iter_mut())
    }

    fn update_next_hit(&mut self) {
        self.next_hit = self.finders().map(|f| f.next).min().unwrap_or(NONE);
    }

    /// The hits in one stretch: the slow path of [`RowObserver::content`].
    #[inline(never)]
    fn take_hits<U: Units>(&mut self, view: &View<'_, U>, segment: Segment) {
        let delimiter = self.delimiter;
        // Every hit in the stretch is in the row being scanned.
        let mut any = false;
        for finder in self.finders() {
            any |= finder.take(view, delimiter, segment);
        }
        self.row_flagged |= any;
        self.update_next_hit();
    }

    /// The diagnostics of the rows finished so far.
    pub(crate) fn report(&self, rows: usize, complete: bool) -> Report {
        let mut diagnostics = Vec::new();
        let mut push = |d: Option<Diagnostic>| diagnostics.extend(d);

        push(self.unterminated.map(|at| Diagnostic {
            kind: DiagnosticKind::UnterminatedQuote,
            count: 1,
            first: vec![at],
        }));
        push(self.ragged_rows());
        push(
            self.text_after_quote
                .diagnostic(DiagnosticKind::TextAfterClosingQuote, rows),
        );
        let field_kind = |finder: &Option<Finder>, kind| {
            finder.as_ref().and_then(|f| f.tally.diagnostic(kind, rows))
        };
        push(field_kind(&self.invalid, DiagnosticKind::InvalidEncoding));
        push(field_kind(&self.nul, DiagnosticKind::NulBytes));
        push(self.mixed_line_endings());
        push(
            self.blank_lines
                .diagnostic(DiagnosticKind::BlankLines, rows),
        );
        if self.diagnostics.dialect().bom_len > 0 {
            push(Some(Diagnostic {
                kind: DiagnosticKind::BomPresent,
                count: 1,
                first: vec![Location { row: 0, offset: 0 }],
            }));
        }
        Report {
            diagnostics,
            rows,
            complete,
        }
    }

    /// Rows whose field count isn't the most common one.
    fn ragged_rows(&self) -> Option<Diagnostic> {
        let (mode, mode_rows) = self.mode?;
        let count = usize::try_from(self.non_blank - mode_rows).unwrap_or(usize::MAX);
        let first: Vec<Location> = self
            .candidates
            .iter()
            .filter(|c| c.fields != mode)
            .map(|c| c.location)
            .take(MAX_LOCATIONS)
            .collect();
        debug_assert_eq!(first.len(), count.min(MAX_LOCATIONS), "ragged rows kept");
        (count > 0).then_some(Diagnostic {
            kind: DiagnosticKind::RaggedRows,
            count,
            first,
        })
    }

    /// Rows whose line ending isn't the most common one. Line endings are
    /// counted when their rows end, so every one counted is in a finished
    /// row.
    fn mixed_line_endings(&self) -> Option<Diagnostic> {
        // The most common; a tie goes to the one seen first.
        let dominant = (0..3)
            .filter(|&i| self.line_endings[i].count > 0)
            .max_by_key(|&i| {
                (
                    self.line_endings[i].count,
                    std::cmp::Reverse(self.first_seen[i]),
                )
            })?;
        let others = (0..3).filter(|&i| i != dominant);
        let count = others.clone().map(|i| self.line_endings[i].count).sum();
        // Each tally has its own first locations, so the first of all the
        // others are among them.
        let mut first: Vec<Location> = others
            .flat_map(|i| self.line_endings[i].first.iter().copied())
            .collect();
        first.sort_unstable();
        first.truncate(MAX_LOCATIONS);
        (count > 0).then_some(Diagnostic {
            kind: DiagnosticKind::MixedLineEndings,
            count,
            first,
        })
    }
}

/// Where a line ending's tally is in [`Collector::line_endings`].
fn ending_index(line_ending: LineEnding) -> usize {
    match line_ending {
        LineEnding::Lf => 0,
        LineEnding::Crlf => 1,
        LineEnding::Cr => 2,
    }
}

impl RowObserver for Collector {
    /// The finders look past the end of a chunk: up to 3 bytes for the rest
    /// of a UTF-8 sequence, or 2 for the low surrogate after a high one
    /// ([`Target::lookahead`]). One more keeps it a whole UTF-16 unit.
    const LOOKAHEAD: usize = 4;

    fn chunk<U: Units>(&mut self, view: &View<'_, U>, to: usize, len: usize) {
        for finder in self.finders() {
            finder.extend(view, to, len);
        }
        self.update_next_hit();
    }

    #[inline]
    fn content<U: Units>(&mut self, view: &View<'_, U>, segment: Segment) {
        // The common case, a stretch with nothing in it: one comparison.
        if self.next_hit < segment.to {
            self.take_hits(view, segment);
        }
    }

    fn text_after_quote(&mut self, row: usize, offset: usize) {
        self.text_after_quote.push_in_row(Location { row, offset });
        self.row_flagged = true;
    }

    fn unterminated_quote(&mut self, row: usize, offset: usize) {
        self.unterminated = Some(Location { row, offset });
        self.row_flagged = true;
    }

    /// The row-level kinds.
    ///
    /// **Ragged rows** depend on the most common field count, which isn't
    /// final until the end, so the rows that might be ragged are kept. Not
    /// all of them: row `p` with `c` fields can be among the first
    /// [`MAX_LOCATIONS`] ragged rows, whichever count `m` ends up most
    /// common, only if `c != m` and fewer than `MAX_LOCATIONS` rows before
    /// it don't have `m` fields. Once there are more than
    /// [`KEEP_ALL_ROWS`] rows, at most one count can satisfy that, a strict
    /// majority, which must then be the current mode. So a row is kept if
    /// it is among the first `KEEP_ALL_ROWS`, or if its count isn't the
    /// mode's and at most `MAX_LOCATIONS` rows so far differ from the mode.
    /// That keeps at most `KEEP_ALL_ROWS + MAX_LOCATIONS` rows, and the
    /// first ragged rows are among them for the final mode and for the mode
    /// of every prefix (which is what a report made while indexing uses).
    #[inline]
    fn row(&mut self, facts: &RowFacts, counts: &FieldCounts) {
        let start = Location {
            row: facts.row,
            offset: facts.span.start,
        };
        let blank = facts.span.is_empty();
        let code = marks::code(facts.fields, blank, self.row_flagged);
        self.new_codes.push(code);
        if code & marks::WIDE == marks::WIDE && !blank {
            // Neither can exceed the file's length, which fits in a `u32`.
            let row = u32::try_from(facts.row).unwrap_or(u32::MAX);
            let delimiters = u32::try_from(facts.fields - 1).unwrap_or(u32::MAX);
            self.new_wide.push((row, delimiters));
        }
        self.row_flagged = false;
        if blank {
            self.blank_lines.push(start);
        } else if let Some((mode, mode_rows)) = counts.leader() {
            // `counts` already includes this row.
            let rows = counts.total();
            let keep = rows <= KEEP_ALL_ROWS
                || (facts.fields != mode && mode_rows + MAX_LOCATIONS as u64 >= rows);
            if keep {
                self.candidates.push(Candidate {
                    location: start,
                    fields: facts.fields,
                });
            }
            self.mode = Some((mode, mode_rows));
            self.non_blank = rows;
        }
        if let Some(line_ending) = facts.line_ending {
            let i = ending_index(line_ending);
            self.line_endings[i].push(Location {
                row: facts.row,
                offset: facts.span.end,
            });
            if self.first_seen[i].is_none() {
                self.first_seen[i] = Some(self.seen);
                self.seen += 1;
            }
        }
    }

    fn published(&mut self, rows: usize, done: bool) {
        let report = self.report(rows, done);
        let mode = self.mode.map(|(fields, _)| fields);
        self.diagnostics
            .publish(report, &mut self.new_codes, &mut self.new_wide, mode);
    }
}
