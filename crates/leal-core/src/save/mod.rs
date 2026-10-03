//! Saving (DESIGN §3.7, task 2.2): the splice writer's rules.
//!
//! A save writes the file it read, byte for byte, with **splices**: each
//! edited row's changed bytes in place of its old ones. Everything outside
//! a splice is copied from the snapshot as it is, so an unedited byte can't
//! change (F1, F2, F4). [`Document::save`] runs it as a job; this module
//! holds what it writes and why, independently of where the bytes come
//! from, so its rules can be tested on their own:
//!
//! 1. An **untouched row** is copied byte for byte, line ending included.
//! 2. An **edited row** keeps its untouched fields' raw bytes, its
//!    delimiters and its line ending. Each edited field becomes its new
//!    value, encoded in the file's encoding and quoted if the value needs
//!    it (it holds the delimiter, `"`, CR or LF), if the field was quoted,
//!    or if the file quotes every field ([`field_bytes`]): one splice per
//!    field whose bytes change.
//! 3. An edited **hatched cell** (past the end of a short or blank row,
//!    ADR-0005 decision 2) gives the row the delimiters needed to reach it,
//!    then its value, at the end of the row before its line ending: one
//!    insert. Cells in between get no bytes at all.
//! 4. Two fixes keep a reopen reading the same rows (ADR-0004 decisions 6,
//!    7 and 10), and make the splice the whole row, line ending included:
//!    a row whose bytes would be empty (its only field emptied) is written
//!    `""` ([`Fix::EmptyRowQuoted`]), and in a file without a BOM, a first
//!    field that would start with BOM-like bytes (`EF BB BF`, `FF FE`,
//!    `FE FF`) is quoted ([`Fix::BomLikeQuoted`]).
//!
//! The trailing newline is the last row's own, so it is kept as it was.
//! These are the save oracle's rules (`leal_testkit::save`) for cell edits,
//! and the property tests compare the two, splice for splice. Row and
//! column inserts and deletes (task 2.4) add the rules for new rows and
//! fields, and the CR/LF split (ADR-0004 decision 10), which only they can
//! need: an edited row never starts with LF.
//!
//! **Encoding** (DESIGN §3.7, F5, task 2.3). Each edited value is written
//! in the file's encoding ([`encode`]): UTF-8 holds any value; a
//! single-byte encoding (all of ADR-0005 decision 5's) holds the
//! characters it has a byte for, and a value with any other is refused
//! before anything is written, naming every such cell
//! ([`SaveError::Unencodable`]). Nothing is ever substituted. The app can
//! ask before an edit is committed ([`encode`], or
//! [`Document::unencodable`](crate::document::Document::unencodable)).
//! UTF-16 files are read-only in v1 ([`SaveError::ReadOnly`]).
//!
//! **Save As UTF-8** ([`SaveKind::SaveAsUtf8`], ADR-0008 decision 7)
//! writes the same rows in UTF-8 from a file in any encoding: the same
//! splices, in UTF-8, with every byte between them converted from the
//! file's encoding as it is copied (`Transcoder`), a stretch at a time,
//! and the BOM a UTF-8 one if the file had a BOM. Line endings, quoting and
//! delimiters are the file's own, in meaning: they are ASCII in UTF-8 and
//! convert one to one. An unedited field whose bytes aren't text in the
//! file's encoding (an unpaired surrogate or a final odd byte in UTF-16, a
//! byte a single-byte encoding leaves unassigned) can't be converted, and
//! the save stops naming those cells ([`SaveError::Unconvertible`]); the
//! user can edit them and try again. From UTF-8 it is Save As, with the
//! attribute set.
//!
//! **Attributes** ([`AttributePlan`], ADR-0004 decision 11, ADR-0005
//! decision 1, ADR-0008 decision 8): what a save sets the file's
//! `com.apple.TextEncoding` and Leal's interpretation attribute to, so a
//! reopen reads the file the way the document did.
//!
//! **Size.** Leal reads files of up to [`MAX_FILE_BYTES`] (DESIGN §1: files
//! over 4 GiB are a non-goal; the index's offsets are 32-bit). A save whose
//! output would be larger is refused before anything is written
//! ([`SaveError::TooLarge`]): Leal couldn't reopen the file, which breaks
//! the reopen guarantee (ADR-0004 decision 10), and F5 says a save
//! succeeds exactly or stops with an explanation.
//!
//! [`Document::save`]: crate::document::Document::save

mod attributes;
mod encode;
mod quoting;
#[cfg(test)]
mod tests;

pub use attributes::AttributePlan;
pub(crate) use attributes::{AttributeFacts, needs_census};
pub(crate) use encode::Transcoder;
pub use encode::{Unencodable, encode};
pub(crate) use quoting::ColumnQuoting;

use std::borrow::Cow;
use std::fmt;
use std::io;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crate::dialect::{Encoding, LineEnding, QUOTE};
use crate::document::FirstScreen;
use crate::index::MAX_FILE_BYTES;
use crate::rows::FieldSpan;
pub use crate::source::Placed;
use crate::source::{OriginalStatus, ReadError, VolumeInfo};
use std::time::SystemTime;

/// One splice: the file's bytes in `range` (offsets into the file as it was
/// read, BOM included) are replaced with `bytes`. An empty range inserts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    /// What is replaced.
    pub range: Range<usize>,
    /// What replaces it.
    pub bytes: Vec<u8>,
}

/// An extra change a save makes so that a reopen reads the same rows
/// (ADR-0004 decisions 6, 7 and 10). Rows are output rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// A row whose bytes would have been empty is written as `""`
    /// (decision 6), so it stays a row with one empty field.
    EmptyRowQuoted {
        /// The row.
        row: usize,
    },
    /// The first field was quoted because it would start with BOM-like
    /// bytes (decisions 7 and 10).
    BomLikeQuoted,
}

/// What a save writes over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveKind {
    /// **Save**: over the user's file, after checking that it is still the
    /// one Leal opened or last saved (ADR-0008 decision 9), keeping its
    /// permissions, extended attributes and Finder metadata (DESIGN §3.7).
    Save,
    /// **Save As**: a new file at another place (or over another file
    /// there), which the document then is. It can save a document whose
    /// bytes aren't all there: only its complete, trusted rows (ADR-0008
    /// decision 6).
    SaveAs,
    /// **Save As UTF-8** (ADR-0008 decision 7): Save As, writing the
    /// document in UTF-8 whatever the file's encoding (see the module docs).
    /// The way out of a UTF-16 file (read-only in v1), and of a value a
    /// single-byte encoding can't hold. Like Save As, from an incomplete
    /// document it writes the complete, trusted rows.
    SaveAsUtf8,
}

impl SaveKind {
    /// The encoding a save of this kind writes, for a document in
    /// `encoding`.
    #[must_use]
    pub fn writes(self, encoding: Encoding) -> Encoding {
        match self {
            SaveKind::Save | SaveKind::SaveAs => encoding,
            SaveKind::SaveAsUtf8 => Encoding::Utf8,
        }
    }

    /// Whether it writes to a new place (Save As, of either kind).
    #[must_use]
    pub fn is_save_as(self) -> bool {
        self != SaveKind::Save
    }
}

/// How many cells a refusal names at most ([`SaveError::Unconvertible`]):
/// as many as a diagnostic locates (DESIGN §3.5).
pub const MAX_NAMED_CELLS: usize = 1000;

/// What [`Document::save`](crate::document::Document::save) is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SaveRequest {
    /// Where to write: the user's file for [`SaveKind::Save`] (where it is
    /// now, [`OriginalStatus::path`]), the chosen place for
    /// [`SaveKind::SaveAs`]. A symbolic link there is followed, so the link
    /// stays a link.
    pub destination: PathBuf,
    /// Save or Save As.
    pub kind: SaveKind,
    /// An empty folder on the destination's volume to write the new file in
    /// before it is renamed into place: the app passes the one
    /// `FileManager.url(for: .itemReplacementDirectory, …, appropriateFor:)`
    /// made, which a sandboxed app may write to. The save takes it over and
    /// deletes it. With `None`, a hidden folder in the destination's own
    /// folder.
    pub folder: Option<PathBuf>,
    /// What the app knows about the destination's volume, for the snapshot
    /// of the saved file the document reads from then on (a clone there, or
    /// a copy; see [`Source::open_on`](crate::source::Source::open_on)).
    pub volume: VolumeInfo,
    /// For Save: the user agreed to write over a file that changed
    /// elsewhere (the app asked, from [`OriginalStatus::diverged`] or a
    /// refusal with [`SaveError::ChangedElsewhere`]).
    pub overwrite_changed: bool,
    /// How many rows the first screen of the saved file has
    /// ([`Saved::reread`]), as for opening it.
    pub first_screen_rows: usize,
    /// The most characters of each of its cells.
    pub max_chars: usize,
}

impl SaveRequest {
    /// A request to save to `destination`, with nothing else given: a hidden
    /// folder next to it, nothing known about its volume, no agreement to
    /// write over a changed file, and the default first screen.
    #[must_use]
    pub fn new(destination: impl Into<PathBuf>, kind: SaveKind) -> SaveRequest {
        let screen = crate::document::OpenOptions::default();
        SaveRequest {
            destination: destination.into(),
            kind,
            folder: None,
            volume: VolumeInfo::default(),
            overwrite_changed: false,
            first_screen_rows: screen.first_screen_rows,
            max_chars: screen.max_chars,
        }
    }
}

/// What a save writes: the splices, and how much of the file. The save
/// makes it as it writes, in file order; `Document::save_plan` (tests and
/// benchmarks only) collects one without writing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavePlan {
    pub(crate) splices: Vec<Splice>,
    /// The snapshot is written up to here: all of it, or the end of its
    /// last trusted row.
    pub(crate) end: usize,
    pub(crate) rows: usize,
    pub(crate) complete: bool,
    pub(crate) fixes: Vec<Fix>,
    pub(crate) len: u64,
    pub(crate) skipped: Vec<(usize, usize)>,
}

impl SavePlan {
    /// The splices, in file order, none overlapping.
    #[must_use]
    pub fn splices(&self) -> &[Splice] {
        &self.splices
    }

    /// The extra changes made to keep the rows (ADR-0004 decisions 6, 7
    /// and 10): the rows written `""`, in order, then the BOM-like first
    /// field, as the save oracle lists them.
    #[must_use]
    pub fn fixes(&self) -> &[Fix] {
        &self.fixes
    }

    /// How many rows are written.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Whether every row of the file is written. `false` for a document
    /// whose bytes aren't all there (a drive or share disconnected, the
    /// file deleted on its share or changed while it was read): only its
    /// complete, trusted rows are (ADR-0008 decision 6).
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// How long the new file is, in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the new file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The edited cells (row, column) on rows that aren't written, because
    /// they are past the trusted rows of an incomplete document: their
    /// edits aren't saved, and the app names them.
    #[must_use]
    pub fn skipped_edits(&self) -> &[(usize, usize)] {
        &self.skipped
    }
}

/// A finished save.
#[derive(Clone, Debug)]
pub struct Saved {
    /// Where the file was written (the destination, a symbolic link
    /// followed), its name as the volume spells it (on a volume that
    /// ignores case, the name the file there already had).
    pub path: PathBuf,
    /// Its length in bytes.
    pub len: u64,
    /// How many rows it has.
    pub rows: usize,
    /// Whether it has every row of the document. `false` for Save As from
    /// an incomplete document: only the complete, trusted rows were written
    /// (ADR-0008 decision 6). The app says "about `rows` of
    /// `estimated_rows` rows".
    pub complete: bool,
    /// The document's row count, or its estimate while the rest of the file
    /// is unknown, for that message.
    pub estimated_rows: usize,
    /// Edited cells that weren't written, because their rows weren't.
    pub skipped_edits: Vec<(usize, usize)>,
    /// Cells edited while the save ran, after it took its snapshot of the
    /// edits ([`SaveProgress::snapshot_version`]): not in the file, and
    /// carried over to the document's new reading as unsaved edits, so the
    /// document is still dirty. If the file couldn't be read back, the
    /// cells edited since the snapshot, still unsaved in the old reading.
    pub edits_during_save: Vec<(usize, usize)>,
    /// The extra changes made to keep the rows.
    pub fixes: Vec<Fix>,
    /// The attributes the file was given.
    pub attributes: AttributePlan,
    /// Metadata of the old file that couldn't be given to the new one and
    /// was skipped (ADR-0012 decision 1): extended attributes by name, or
    /// "access control list", "permissions", "flags", "creation date". For
    /// the app's log.
    pub skipped_metadata: Vec<String>,
    /// How the new file went into place: swapped with the old one and the
    /// old one checked, or renamed over it on a volume that can't swap.
    pub placed: Placed,
    /// The old file, kept rather than deleted, if the swap took out one
    /// that couldn't be checked, or wasn't the one checked and couldn't be
    /// swapped back: it may be another app's version. Next to the file
    /// (`a.csv`'s as `a (replaced, kept by Leal).csv`), or, if it couldn't
    /// be moved there, in Leal's folder beside it, never cleaned up. The
    /// app tells the user.
    pub kept: Option<PathBuf>,
    /// The new file's modification date, for `NSDocument`'s
    /// `fileModificationDate` (ADR-0012).
    pub modified: Option<SystemTime>,
    /// The file as the watcher sees it now: the new one, unchanged.
    pub original: OriginalStatus,
    /// The first screen of the document as it reads now, from the file it
    /// wrote (ADR-0008 decision 1): a new reading, with a new generation.
    /// `None` if the saved file couldn't be read back (it is saved; the
    /// document still reads the old snapshot, with its edits, and a later
    /// save writes the same bytes); [`reread_error`](Self::reread_error)
    /// says why.
    pub reread: Option<FirstScreen>,
    /// Why the saved file couldn't be read back, for the log.
    pub reread_error: Option<String>,
}

/// What a save is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SavePhase {
    /// Waiting for another save of the document to finish.
    #[default]
    Queued,
    /// Waiting for the index pass to finish (for a file on a removable
    /// drive or a share, its copy): `written` and `total` are the pass's
    /// bytes.
    Indexing,
    /// The edits' snapshot taken
    /// ([`SaveProgress::snapshot_version`]): checking the file, then
    /// writing the new file.
    Writing,
    /// Giving it the old file's metadata and flushing it.
    Flushing,
    /// Putting it in place and reading it back.
    Replacing,
    /// Done, however it ended.
    Finished,
}

/// How far a save has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveProgress {
    /// What it is doing.
    pub phase: SavePhase,
    /// Bytes written so far (while indexing, indexed so far).
    pub written: u64,
    /// Bytes to write in all (while indexing, the file's length); 0 until
    /// known.
    pub total: u64,
    /// The document's edit version ([`Document::edit_version`]) when the
    /// save took its snapshot of the edits, once it has (the phase is
    /// [`SavePhase::Writing`] from then on). Edits after it aren't in the
    /// file: the app's change-count token for "saved" is taken at it.
    ///
    /// [`Document::edit_version`]: crate::document::Document::edit_version
    pub snapshot_version: Option<u64>,
}

/// Why a save didn't happen. The user's file is then untouched, and
/// anything written for it is deleted.
#[derive(Debug)]
pub enum SaveError {
    /// UTF-16 files are read-only in v1 (DESIGN §4.3): Save As UTF-8
    /// ([`SaveKind::SaveAsUtf8`]) is the way out.
    ReadOnly,
    /// These cells (row, column) hold a character the file's encoding
    /// can't represent (F5: nothing is substituted). Checked before
    /// anything is written. The app names them and offers Save As UTF-8
    /// (DESIGN §3.7).
    Unencodable {
        /// The file's encoding.
        encoding: Encoding,
        /// Every such cell, in order.
        cells: Vec<(usize, usize)>,
    },
    /// Save As UTF-8 (ADR-0008 decision 7): these unedited cells (row,
    /// column) hold bytes that aren't text in the file's encoding (an
    /// unpaired surrogate or a final odd byte in UTF-16, a byte a
    /// single-byte encoding leaves unassigned), so they can't be converted
    /// (F5). The user can edit them and try again.
    Unconvertible {
        /// The file's encoding.
        encoding: Encoding,
        /// The cells, in file order: the first [`MAX_NAMED_CELLS`].
        cells: Vec<(usize, usize)>,
        /// Whether there are more than those.
        more: bool,
    },
    /// The new file would be `len` bytes, more than Leal reads
    /// ([`MAX_FILE_BYTES`]), so Leal couldn't open it again.
    TooLarge {
        /// Its length.
        len: u64,
    },
    /// Save over the user's file isn't possible, because Leal doesn't have
    /// all of it: its drive or share disconnected, or it was deleted on its
    /// share, before it was all read, or it changed while it was read.
    /// Save As can save the rows Leal trusts (ADR-0008 decision 6).
    Incomplete,
    /// The file's volume isn't mounted, so Save can't write there.
    Unavailable,
    /// The file on disk isn't the one Leal opened or last saved (ADR-0008
    /// decision 9): another app changed or replaced it. The app asks, and
    /// saves again with [`SaveRequest::overwrite_changed`] if the user
    /// agrees.
    ChangedElsewhere,
    /// Nothing is at the destination any more for Save: the file was
    /// deleted or moved away. Save As can save it.
    Missing,
    /// The file was renamed a moment ago and may be part of another app's
    /// save: Save waits for where it is to settle. Try again in a moment.
    Moving,
    /// Leal may not write the user's file (its permissions or access
    /// control list): a rename would replace it anyway, so Save refuses
    /// first (ADR-0012 decision 1). The app offers Duplicate.
    NotWritable,
    /// The user's file is locked (Finder's Locked, or append-only). The
    /// app offers to unlock it.
    Locked,
    /// Something other than a regular file (a folder, a pipe, a device) is
    /// at the destination: a save never replaces it.
    NotAFile,
    /// The document's bytes couldn't be read (a file on a removable drive
    /// or a share).
    Read(ReadError),
    /// Writing the new file, or renaming it into place, failed: a full
    /// disk, no permission, a locked file.
    Write {
        /// What failed, in English, for logs.
        step: &'static str,
        /// The error, with its errno.
        error: io::Error,
    },
    /// Rows or columns have been inserted or deleted, which Leal can't
    /// save yet (task 2.4c): nothing was written. The app keeps the edits.
    RowsChanged,
    /// The save was cancelled.
    Cancelled,
    /// Anything else, such as a panic in the save. English, for logs.
    Failed(String),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::ReadOnly => f.write_str("UTF-16 files are read-only"),
            SaveError::Unencodable { encoding, cells } => write!(
                f,
                "cells {cells:?} hold text that can't be saved in {}",
                encoding.iana_name()
            ),
            SaveError::Unconvertible {
                encoding,
                cells,
                more,
            } => write!(
                f,
                "cells {cells:?}{} hold bytes that aren't {} text, so they can't be converted",
                if *more { " and more" } else { "" },
                encoding.iana_name()
            ),
            SaveError::TooLarge { len } => write!(
                f,
                "the file would be {len} bytes; Leal reads files of up to {MAX_FILE_BYTES} bytes"
            ),
            SaveError::Incomplete => {
                f.write_str("Leal doesn't have all of the file, so it can only Save As")
            }
            SaveError::Unavailable => f.write_str("the file's volume isn't mounted"),
            SaveError::ChangedElsewhere => f.write_str("the file changed elsewhere"),
            SaveError::Missing => f.write_str("the file isn't there any more"),
            SaveError::Moving => f.write_str("the file is being moved"),
            SaveError::NotWritable => f.write_str("Leal may not write the file"),
            SaveError::Locked => f.write_str("the file is locked"),
            SaveError::NotAFile => f.write_str("something other than a file is there"),
            SaveError::Read(error) => error.fmt(f),
            SaveError::Write { step, error } => write!(f, "{step}: {error}"),
            SaveError::RowsChanged => {
                f.write_str("rows or columns were inserted or deleted, which can't be saved yet")
            }
            SaveError::Cancelled => f.write_str("the save was cancelled"),
            SaveError::Failed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SaveError {}

impl From<ReadError> for SaveError {
    fn from(error: ReadError) -> Self {
        SaveError::Read(error)
    }
}

/// Whether a field holding `value` must be quoted wherever it is written:
/// it holds the delimiter, `"`, CR or LF (§3.7). On the encoded bytes; the
/// structural bytes are ASCII in every encoding Leal saves.
#[must_use]
pub fn needs_quotes(value: &[u8], delimiter: u8) -> bool {
    value
        .iter()
        .any(|&b| b == delimiter || b == QUOTE || b == b'\r' || b == b'\n')
}

/// The bytes §3.7 rule 2 writes for an edited field: `value` encoded, and
/// quoted (each `"` doubled) if it needs it or `quoted` (the field was
/// quoted, or the file quotes every field). For the encodings a save
/// writes, which are all ASCII-compatible (not UTF-16, read-only in v1).
///
/// # Errors
///
/// [`Unencodable`] if `value` can't be encoded ([`encode`]).
pub fn field_bytes(
    value: &str,
    encoding: Encoding,
    delimiter: u8,
    quoted: bool,
) -> Result<Cow<'_, [u8]>, Unencodable> {
    debug_assert!(encoding.is_ascii_compatible(), "{encoding:?} isn't written");
    let encoded = encode(value, encoding)?;
    if quoted || needs_quotes(&encoded, delimiter) {
        Ok(Cow::Owned(quote(&encoded)))
    } else {
        Ok(encoded)
    }
}

/// `"` + `value` with each `"` doubled + `"`.
fn quote(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 2);
    out.push(QUOTE);
    for &b in value {
        if b == QUOTE {
            out.push(QUOTE);
        }
        out.push(b);
    }
    out.push(QUOTE);
    out
}

/// The byte order marks a field mustn't start the file with when it has
/// none (ADR-0004 decision 10): UTF-8's, and UTF-16's in either order.
const BOM_LIKE: [&[u8]; 3] = [b"\xEF\xBB\xBF", b"\xFF\xFE", b"\xFE\xFF"];

/// How an edited row is written, as far as the file as a whole decides it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowRules {
    /// The encoding written ([`SaveKind::writes`]).
    pub(crate) encoding: Encoding,
    /// The file's own encoding. Where it isn't the one written (Save As
    /// UTF-8), the row's unedited bytes are converted.
    pub(crate) source: Encoding,
    pub(crate) delimiter: u8,
    /// The file quotes every field (ADR-0004 decision 1), so an edited
    /// hatched cell is quoted too.
    pub(crate) quote_all: bool,
}

impl RowRules {
    /// Whether the row's own bytes are converted (Save As UTF-8 from
    /// another encoding).
    fn converts(self) -> bool {
        self.source != self.encoding
    }

    /// How many bytes a structural character takes in the file: 2 in
    /// UTF-16, otherwise 1.
    fn unit(self) -> usize {
        if self.source.is_ascii_compatible() {
            1
        } else {
            2
        }
    }
}

/// One edited row of the file, as it was read.
#[derive(Clone, Debug)]
pub(crate) struct EditedRow<'a> {
    /// The row, which is also its place in the output.
    pub(crate) row: usize,
    /// The file's bytes from `base` on, holding the row and its line ending.
    pub(crate) bytes: &'a [u8],
    pub(crate) base: usize,
    /// The row's bytes, without its line ending.
    pub(crate) span: Range<usize>,
    pub(crate) line_ending: Option<LineEnding>,
    pub(crate) fields: &'a [FieldSpan],
    /// Its edited cells, by column, sorted.
    pub(crate) cells: &'a [(usize, Arc<str>)],
    /// It is the file's first row, in a file without a BOM.
    pub(crate) first_without_bom: bool,
}

impl EditedRow<'_> {
    fn raw(&self, field: &FieldSpan) -> &[u8] {
        let span = field.span();
        &self.bytes[span.start - self.base..span.end - self.base]
    }

    fn edited(&self, column: usize) -> Option<&str> {
        let at = self.cells.binary_search_by_key(&column, |&(c, _)| c).ok()?;
        Some(&self.cells[at].1)
    }

    /// How many cells the row has now.
    fn len(&self) -> usize {
        let end = self.cells.last().map_or(0, |&(c, _)| c + 1);
        self.fields.len().max(end)
    }

    /// An unedited field's bytes as written: its raw bytes, converted to
    /// the encoding written if that isn't the file's (Save As UTF-8).
    /// `None` if they aren't text in the file's encoding.
    fn original(&self, field: &FieldSpan, rules: RowRules) -> Option<Cow<'_, [u8]>> {
        let raw = self.raw(field);
        if rules.converts() {
            Transcoder::convert(raw, rules.source)
        } else {
            Some(Cow::Borrowed(raw))
        }
    }

    /// Cell `column`'s bytes as written: its raw bytes (converted, for Save
    /// As UTF-8), its new value's, or none (padding before an edited
    /// hatched cell). `Err` with the column if its value can't be encoded,
    /// or its bytes converted.
    fn cell_bytes(&self, column: usize, rules: RowRules) -> Result<Cow<'_, [u8]>, usize> {
        match (self.edited(column), self.fields.get(column)) {
            (Some(value), field) => {
                let quoted = rules.quote_all || field.is_some_and(FieldSpan::quoted);
                field_bytes(value, rules.encoding, rules.delimiter, quoted).map_err(|_| column)
            }
            (None, Some(field)) => self.original(field, rules).ok_or(column),
            (None, None) => Ok(Cow::Borrowed(&[])),
        }
    }

    /// The unedited columns whose bytes can't be converted (Save As UTF-8):
    /// the cells a refusal names when the row can't be written.
    pub(crate) fn unconvertible(&self, rules: RowRules) -> Vec<usize> {
        (0..self.fields.len())
            .filter(|&column| self.edited(column).is_none())
            .filter(|&column| self.original(&self.fields[column], rules).is_none())
            .collect()
    }

    /// The row's content (its bytes without the line ending) as written,
    /// cell by cell, up to `limit` bytes.
    fn content(&self, rules: RowRules, limit: usize) -> Result<Vec<u8>, usize> {
        let mut content = Vec::new();
        for column in 0..self.len() {
            if content.len() >= limit {
                break;
            }
            if column > 0 {
                content.push(rules.delimiter);
            }
            content.extend_from_slice(&self.cell_bytes(column, rules)?);
        }
        Ok(content)
    }

    /// The end of the row's line ending, in the file.
    pub(crate) fn end(&self, rules: RowRules) -> usize {
        self.span.end
            + self
                .line_ending
                .map_or(0, |ending| ending.bytes().len() * rules.unit())
    }
}

/// The splices that write `row` (see the module docs), added to `splices`,
/// and any fix, added to `fixes`. On error, the columns whose values can't
/// be encoded, and nothing is added.
pub(crate) fn row_splices(
    row: &EditedRow<'_>,
    rules: RowRules,
    splices: &mut Vec<Splice>,
    fixes: &mut Vec<Fix>,
) -> Result<(), Vec<usize>> {
    let unencodable: Vec<usize> = row
        .cells
        .iter()
        .filter(|(column, _)| row.cell_bytes(*column, rules).is_err())
        .map(|&(column, _)| column)
        .collect();
    if !unencodable.is_empty() {
        return Err(unencodable);
    }
    let len = row.len();
    // ADR-0004 decision 6: a row with no bytes would read back as a blank
    // line, or vanish at the end of the file. An edited row is never an
    // original blank line (setting a blank line's cell back to "" removes
    // the edit), so it is written `""`.
    let empty = len == 1 && row.cell_bytes(0, rules).is_ok_and(|bytes| bytes.is_empty());
    // ADR-0004 decisions 7 and 10: only the first field of the file can be
    // read as a BOM.
    let bom_like = !empty
        && row.first_without_bom
        && row.content(rules, 3).is_ok_and(|start| {
            BOM_LIKE
                .iter()
                .any(|bom| start.len() >= bom.len() && start.starts_with(bom))
        });
    if empty || bom_like {
        let ending = row.line_ending.map_or(&b""[..], LineEnding::bytes);
        let mut bytes = if empty {
            fixes.push(Fix::EmptyRowQuoted { row: row.row });
            b"\"\"".to_vec()
        } else {
            fixes.push(Fix::BomLikeQuoted);
            let content = row.content(rules, usize::MAX).map_err(|c| vec![c])?;
            let first = row.cell_bytes(0, rules).map_err(|c| vec![c])?.len();
            let mut quoted = quote(&content[..first]);
            quoted.extend_from_slice(&content[first..]);
            quoted
        };
        bytes.extend_from_slice(ending);
        splices.push(Splice {
            range: row.span.start..row.end(rules),
            bytes,
        });
        return Ok(());
    }
    // Rule 2: each edited field whose bytes change.
    for &(column, _) in row.cells {
        let Some(field) = row.fields.get(column) else {
            break;
        };
        let bytes = row.cell_bytes(column, rules).map_err(|c| vec![c])?;
        if row
            .original(field, rules)
            .is_none_or(|original| *bytes != *original)
        {
            splices.push(Splice {
                range: field.span(),
                bytes: bytes.into_owned(),
            });
        }
    }
    // Rule 3: the hatched cells, at the end of the row.
    if len > row.fields.len() {
        let mut appended = Vec::new();
        for column in row.fields.len()..len {
            appended.push(rules.delimiter);
            appended.extend_from_slice(&row.cell_bytes(column, rules).map_err(|c| vec![c])?);
        }
        splices.push(Splice {
            range: row.span.end..row.span.end,
            bytes: appended,
        });
    }
    Ok(())
}

/// The new file's length so far: `at` bytes of the snapshot, with splices
/// that made it `delta` bytes longer (or shorter). `Err` with it if it is
/// more than Leal reads ([`MAX_FILE_BYTES`]): the save stops there, before
/// the new file goes anywhere (ADR-0012 decision 2).
pub(crate) fn checked_len(at: usize, delta: i64) -> Result<u64, u64> {
    let len = i128::try_from(at).unwrap_or(i128::MAX) + i128::from(delta);
    let len = u64::try_from(len.max(0)).unwrap_or(u64::MAX);
    if len > u64::try_from(MAX_FILE_BYTES).unwrap_or(u64::MAX) {
        Err(len)
    } else {
        Ok(len)
    }
}
