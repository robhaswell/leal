//! Dialect and encoding detection (DESIGN §3.2): how to read a file.
//!
//! Detection is a pure function of the file's bytes, the values of its two
//! extended attributes, and anything the user chose. It never reads files
//! or attributes itself and never changes a byte; the `source` module (task
//! 1.1) supplies the bytes and attribute values.
//!
//! It runs twice (DESIGN §3.10, ADR-0005 decision 4):
//!
//! - [`detect`] at first paint (P0). It reads only the first
//!   [`FIRST_PAINT_BYTES`] of the file, and decides everything the grid
//!   needs: encoding, BOM, delimiter, quote, line endings and header row.
//! - [`review`] afterwards (P2), over the whole file. It applies the
//!   whole-file encoding rule, checks the delimiter on samples from the
//!   middle and end, and finds the trailing newline. A disagreement becomes
//!   a *suggestion* for the app to show; nothing is re-decided silently.
//!
//! # Encoding order
//!
//! 1. The user's choice (**Reopen with encoding…**), if any.
//! 2. A BOM.
//! 3. The `com.apple.TextEncoding` attribute (ADR-0004 decision 11). A
//!    UTF-8 or Windows-1252 attribute is always honoured. Another supported
//!    single-byte encoding is honoured only if the bytes decode under it. A
//!    UTF-16 attribute without a UTF-16 BOM, an unsupported encoding and an
//!    unreadable value are ignored, each with a [`Note`].
//! 4. The guess, ADR-0003 decision 1 applied to the first 64 KB.
//!
//! # Delimiter and header order
//!
//! 1. The user's choice.
//! 2. Leal's interpretation attribute (ADR-0005 decision 1), if the file
//!    still parses sensibly with it. While the file matches the attribute's
//!    [`Fingerprint`] it is the file Leal
//!    saved, so it does. Otherwise something else has changed it, and the
//!    remembered delimiter must give field counts at least as consistent as
//!    the best guess does. If it doesn't, the whole attribute (header
//!    included) is ignored with a [`Note`].
//! 3. The guess: the delimiter with the most consistent field count, and
//!    the header heuristic (see `header.rs`).
//!
//! A file with no delimiter (one column, or empty) gets `,`, and a file with
//! fewer than two non-blank rows has no header (0.2 notes; confirmed in the
//! 1.2 notes).

mod delimiter;
mod encoding;
mod header;
mod rows;
mod units;

use std::fmt;

use crate::attributes::{Fingerprint, Interpretation, TextEncodingError, parse_text_encoding};
use crate::dialect::{Bom, Delimiter, Encoding, LineEnding, QUOTE};
use delimiter::{Scores, best, score_of, scores, splits, splits_from};
use encoding::{Census, decodes};
use rows::{Row, Rows, field_values, whole_rows};
use units::Units;

/// How much of the start of a file first paint reads (DESIGN §3.2).
pub const FIRST_PAINT_BYTES: usize = 64 * 1024;

/// The raw values of the file's extended attributes, as the `source`
/// module reads them. `None` means the attribute isn't there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hints<'a> {
    /// `com.apple.TextEncoding`, for example `utf-8;134217984`.
    pub text_encoding: Option<&'a [u8]>,
    /// `io.github.robhaswell.leal.interpretation`; the format is in
    /// [`crate::attributes`].
    pub interpretation: Option<&'a [u8]>,
}

/// What the user chose in place of what detection would find (DESIGN
/// §3.2). Choosing changes how the file is read, never its bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Choices {
    /// **Treat as** a delimiter.
    pub delimiter: Option<Delimiter>,
    /// The **Header row** toggle.
    pub header: Option<bool>,
    /// **Reopen with encoding…**. It must agree with the file's BOM: a
    /// file with a BOM can only be read in the BOM's encoding, and UTF-16
    /// only with a UTF-16 BOM.
    pub encoding: Option<Encoding>,
}

/// Where the encoding came from, for the status bar (ADR-0005 decision 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EncodingSource {
    /// The file's byte order mark.
    Bom,
    /// The `com.apple.TextEncoding` attribute.
    Attribute,
    /// The guess from the bytes.
    Guess,
    /// The user's choice.
    User,
}

/// Where the delimiter or header choice came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DialectSource {
    /// Leal's interpretation attribute.
    Attribute,
    /// The guess from the bytes.
    Guess,
    /// The user's choice.
    User,
}

/// Something about the attributes the status bar can mention. Each is
/// ignored, and detection carries on as if it weren't there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Note {
    /// `com.apple.TextEncoding` isn't `name;number`.
    TextEncodingUnreadable,
    /// `com.apple.TextEncoding` names an encoding Leal doesn't read.
    TextEncodingUnsupported {
        /// Its `CFStringEncoding` number.
        cf_string_encoding: u32,
    },
    /// `com.apple.TextEncoding` says UTF-16, but the file has no UTF-16 BOM.
    TextEncodingUtf16WithoutBom,
    /// `com.apple.TextEncoding` names a single-byte encoding in which some
    /// of the file's bytes don't decode.
    TextEncodingDoesNotDecode {
        /// The encoding the attribute names.
        encoding: Encoding,
    },
    /// Leal's interpretation attribute couldn't be read.
    InterpretationUnreadable,
    /// The file has changed since Leal saved the interpretation attribute,
    /// and the remembered delimiter no longer fits it.
    InterpretationNotSensible {
        /// The remembered delimiter.
        delimiter: Delimiter,
    },
}

/// How to read a file, as first paint decides it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    /// The encoding the document uses, for display and saving.
    pub encoding: Encoding,
    /// Where [`Detection::encoding`] came from.
    pub encoding_source: EncodingSource,
    /// The file's BOM. Its bytes belong to no row.
    pub bom: Bom,
    /// The field delimiter.
    pub delimiter: Delimiter,
    /// Where [`Detection::delimiter`] came from.
    pub delimiter_source: DialectSource,
    /// Whether the first row is shown as the header row.
    pub header: bool,
    /// Where [`Detection::header`] came from.
    pub header_source: DialectSource,
    /// The most common line ending in the first 64 KB (ties go to the
    /// first seen), or `None` if no row there has one.
    pub line_ending: Option<LineEnding>,
    /// Whether the first 64 KB has more than one kind of line ending.
    pub mixed_line_endings: bool,
    /// Whether the last row ends with a line ending. `None` if the file is
    /// larger than [`FIRST_PAINT_BYTES`]: [`review`] finds it. A file whose
    /// last field is an unterminated quote has none (its last newline is
    /// inside the field).
    pub trailing_newline: Option<bool>,
    /// Attribute problems, for the status bar.
    pub notes: Vec<Note>,
}

impl Detection {
    /// The quote character, always `"` in v1.
    #[must_use]
    pub const fn quote(&self) -> u8 {
        QUOTE
    }
}

/// A user choice that can't apply to this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceError {
    /// The chosen encoding disagrees with the file's BOM (which is
    /// [`Bom::None`] for a UTF-16 choice on a file without one).
    EncodingDoesNotMatchBom {
        /// The chosen encoding.
        encoding: Encoding,
        /// The file's BOM.
        bom: Bom,
    },
}

impl fmt::Display for ChoiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChoiceError::EncodingDoesNotMatchBom { encoding, bom } => {
                write!(f, "a file with BOM {bom:?} can't be read as {encoding:?}")
            }
        }
    }
}

impl std::error::Error for ChoiceError {}

/// Decides how to read `file` for first paint, from its first
/// [`FIRST_PAINT_BYTES`] only. Pass the whole file (for example the mapped
/// clone): nothing past that limit is read.
///
/// ```
/// use leal_core::detect::{Choices, Hints, detect};
/// use leal_core::dialect::{Delimiter, Encoding};
///
/// let d = detect(b"product;price\nApple;1,20\n", Hints::default(), Choices::default())?;
/// assert_eq!(d.delimiter, Delimiter::Semicolon);
/// assert_eq!(d.encoding, Encoding::Utf8);
/// assert!(d.header);
/// # Ok::<(), leal_core::detect::ChoiceError>(())
/// ```
///
/// # Errors
///
/// [`ChoiceError::EncodingDoesNotMatchBom`] if `choices.encoding`
/// disagrees with the file's BOM.
pub fn detect(file: &[u8], hints: Hints<'_>, choices: Choices) -> Result<Detection, ChoiceError> {
    let cut = file.len() > FIRST_PAINT_BYTES;
    let head = &file[..file.len().min(FIRST_PAINT_BYTES)];
    let bom = Bom::detect(head);
    let body = &head[bom.len()..];
    let mut notes = Vec::new();

    let (encoding, encoding_source) = choose_encoding(
        body,
        cut,
        bom,
        hints.text_encoding,
        choices.encoding,
        &mut notes,
    )?;
    let units = Units::new(body, encoding);

    // The delimiter: chosen, remembered, or guessed.
    let scores = scores(|d| splits(units, d, cut));
    let guess = best(&scores).map(|(d, _)| d);
    let remembered = remembered(file, hints.interpretation, &scores, guess, &mut notes);
    let (delimiter, delimiter_source) = match (choices.delimiter, remembered.delimiter) {
        (Some(d), _) => (d, DialectSource::User),
        (None, Some(d)) => (d, DialectSource::Attribute),
        (None, None) => (guess.unwrap_or(Delimiter::Comma), DialectSource::Guess),
    };

    // Everything else is read under that delimiter.
    let rows = whole_rows(units, delimiter.byte(), cut);
    let (line_ending, mixed_line_endings) = line_endings(rows.iter().map(|r| r.ending));
    let trailing_newline = (!cut).then(|| rows.last().is_some_and(|r| r.ending.is_some()));
    let (header, header_source) = match (choices.header, remembered.header) {
        (Some(h), _) => (h, DialectSource::User),
        (None, Some(h)) => (h, DialectSource::Attribute),
        (None, None) => (guess_header(units, &rows, delimiter), DialectSource::Guess),
    };

    Ok(Detection {
        encoding,
        encoding_source,
        bom,
        delimiter,
        delimiter_source,
        header,
        header_source,
        line_ending,
        mixed_line_endings,
        trailing_newline,
        notes,
    })
}

fn choose_encoding(
    body: &[u8],
    cut: bool,
    bom: Bom,
    text_encoding: Option<&[u8]>,
    chosen: Option<Encoding>,
    notes: &mut Vec<Note>,
) -> Result<(Encoding, EncodingSource), ChoiceError> {
    if let Some(chosen) = chosen {
        let fits = match bom.encoding() {
            Some(e) => e == chosen,
            None => chosen.is_ascii_compatible(),
        };
        if !fits {
            return Err(ChoiceError::EncodingDoesNotMatchBom {
                encoding: chosen,
                bom,
            });
        }
        return Ok((chosen, EncodingSource::User));
    }
    if let Some(e) = bom.encoding() {
        return Ok((e, EncodingSource::Bom));
    }
    if let Some(value) = text_encoding {
        match parse_text_encoding(value) {
            Ok(e @ (Encoding::Utf8 | Encoding::Windows1252)) => {
                return Ok((e, EncodingSource::Attribute));
            }
            Ok(e) if decodes(body, e) => return Ok((e, EncodingSource::Attribute)),
            Ok(e) => notes.push(Note::TextEncodingDoesNotDecode { encoding: e }),
            Err(TextEncodingError::Unreadable) => notes.push(Note::TextEncodingUnreadable),
            Err(TextEncodingError::Unsupported { cf_string_encoding }) => {
                notes.push(Note::TextEncodingUnsupported { cf_string_encoding });
            }
            Err(TextEncodingError::Utf16 { .. }) => notes.push(Note::TextEncodingUtf16WithoutBom),
        }
    }
    Ok((Census::of(body, cut).guess(), EncodingSource::Guess))
}

/// The interpretation attribute's choices, if it is readable and the file
/// is the one Leal saved or its delimiter still fits; otherwise nothing,
/// with a note.
fn remembered(
    file: &[u8],
    value: Option<&[u8]>,
    scores: &Scores,
    guess: Option<Delimiter>,
    notes: &mut Vec<Note>,
) -> Interpretation {
    let Some(value) = value else {
        return Interpretation::default();
    };
    let Ok(interpretation) = Interpretation::parse(value) else {
        notes.push(Note::InterpretationUnreadable);
        return Interpretation::default();
    };
    let unchanged = interpretation.file == Some(Fingerprint::of(file));
    if let Some(d) = interpretation.delimiter
        && !unchanged
        && !fits(scores, d, guess)
    {
        notes.push(Note::InterpretationNotSensible { delimiter: d });
        return Interpretation::default();
    }
    interpretation
}

/// "Still parses sensibly" (ADR-0005 decision 1): the remembered delimiter
/// is the guess, or no delimiter splits the rows, or it splits them at
/// least as consistently as the guess does (counting one field per row as
/// consistent, since a one-column file is a sensible reading).
fn fits(scores: &Scores, remembered: Delimiter, guess: Option<Delimiter>) -> bool {
    let Some(guess) = guess.filter(|g| *g != remembered) else {
        return true;
    };
    match (score_of(scores, remembered), score_of(scores, guess)) {
        (Some(r), Some(g)) => r.at_least_as_consistent_as(&g),
        _ => true,
    }
}

/// The most common of rows' line endings (ties go to the first seen) and
/// whether there is more than one kind.
fn line_endings(
    endings: impl IntoIterator<Item = Option<LineEnding>>,
) -> (Option<LineEnding>, bool) {
    // (ending, count), in order of first appearance.
    let mut counts: Vec<(LineEnding, usize)> = Vec::new();
    for ending in endings.into_iter().flatten() {
        match counts.iter_mut().find(|(e, _)| *e == ending) {
            Some((_, n)) => *n += 1,
            None => counts.push((ending, 1)),
        }
    }
    // `max_by_key` keeps the last of equal counts; reversed, the first.
    let dominant = counts.iter().rev().max_by_key(|(_, n)| *n).map(|(e, _)| *e);
    (dominant, counts.len() > 1)
}

fn guess_header(units: Units<'_>, rows: &[Row], delimiter: Delimiter) -> bool {
    let d = delimiter.byte();
    let Some(first) = rows.first() else {
        return false;
    };
    let first = (!first.is_blank()).then(|| field_values(units, first, d));
    let below: Vec<Vec<String>> = rows[1..]
        .iter()
        .filter(|r| !r.is_blank())
        .take(header::ROWS_LOOKED_AT)
        .map(|r| field_values(units, r, d))
        .collect();
    header::is_header(first.as_deref(), &below)
}

/// What the whole-file check (P2) found. Suggestions are only ever made
/// for a guess; an encoding or delimiter from the BOM, an attribute or the
/// user is not second-guessed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Review {
    /// An encoding the whole file fits better: the whole-file rule
    /// (ADR-0003 decision 1) disagrees with first paint's guess, or some of
    /// the file doesn't decode under the attribute's single-byte encoding.
    /// The app offers "Reopen as …"; it never re-decodes by itself.
    pub encoding_suggestion: Option<Encoding>,
    /// A delimiter the middle and end of the file fit better than the
    /// guessed one ("This file looks semicolon-separated — Switch").
    pub delimiter_suggestion: Option<Delimiter>,
    /// The most common line ending in the whole file (ties go to the first
    /// seen), read with the document's delimiter.
    pub line_ending: Option<LineEnding>,
    /// Whether the whole file has more than one kind of line ending.
    pub mixed_line_endings: bool,
    /// Whether the last row ends with a line ending.
    pub trailing_newline: bool,
}

/// Checks first paint's decisions against the whole of `file` (P2 work,
/// DESIGN §3.10). `detection` is what [`detect`] returned for this file.
///
/// This reads every byte, once for the encoding rule and once for the
/// rows, so it belongs on a background thread.
///
/// ```
/// use leal_core::detect::{Choices, FIRST_PAINT_BYTES, Hints, detect, review};
/// use leal_core::dialect::Encoding;
///
/// // ASCII for the first 64 KB, then a Windows-1252 "é".
/// let mut file = vec![b'a'; FIRST_PAINT_BYTES];
/// file.extend_from_slice(b"\ncaf\xE9\n");
/// let d = detect(&file, Hints::default(), Choices::default())?;
/// assert_eq!(d.encoding, Encoding::Utf8);
/// assert_eq!(review(&file, &d).encoding_suggestion, Some(Encoding::Windows1252));
/// # Ok::<(), leal_core::detect::ChoiceError>(())
/// ```
#[must_use]
pub fn review(file: &[u8], detection: &Detection) -> Review {
    let body = &file[detection.bom.len().min(file.len())..];
    let cut = file.len() > FIRST_PAINT_BYTES;

    let encoding_suggestion = match detection.encoding_source {
        EncodingSource::Guess => {
            Some(Census::of(body, false).guess()).filter(|e| *e != detection.encoding)
        }
        EncodingSource::Attribute
            if !matches!(detection.encoding, Encoding::Utf8 | Encoding::Windows1252)
                && !decodes(body, detection.encoding) =>
        {
            Some(Census::of(body, false).guess())
        }
        _ => None,
    };

    let units = Units::new(body, detection.encoding);
    let delimiter_suggestion = if detection.delimiter_source == DialectSource::Guess && cut {
        later_delimiter(units, detection.delimiter)
    } else {
        None
    };

    // One pass over every row, without keeping them.
    let mut last_ending = None;
    let endings = Rows::new(units, detection.delimiter.byte()).map(|r| {
        last_ending = Some(r.ending);
        r.ending
    });
    let (line_ending, mixed_line_endings) = line_endings(endings);
    Review {
        encoding_suggestion,
        delimiter_suggestion,
        line_ending,
        mixed_line_endings,
        trailing_newline: last_ending.flatten().is_some(),
    }
}

/// The delimiter that samples from the middle and end of the file suggest,
/// if it isn't `in_use` and splits those rows more consistently than
/// `in_use` does.
fn later_delimiter(units: Units<'_>, in_use: Delimiter) -> Option<Delimiter> {
    let len = units.len();
    // The samples are the size of first paint's, in units, and don't
    // overlap first paint's or each other.
    let window = FIRST_PAINT_BYTES / units.width();
    let end_start = len.saturating_sub(window).max(window);
    let middle_start = (len / 2).saturating_sub(window / 2).max(window);
    let middle_end = (middle_start + window).min(end_start);

    let sample = |d: Delimiter| {
        let mut counts = Vec::new();
        if middle_start < middle_end {
            let middle = Units::new(units.bytes(middle_start..middle_end), units.encoding());
            counts.extend(splits_from(middle, d, true));
        }
        if end_start < len {
            counts.extend(splits_from(units.starting_at(end_start), d, false));
        }
        counts
    };
    let scores = scores(sample);
    let (suggested, score) = best(&scores)?;
    if suggested == in_use {
        return None;
    }
    let better = match score_of(&scores, in_use) {
        Some(current) if current.splits() => !current.at_least_as_consistent_as(&score),
        _ => true,
    };
    better.then_some(suggested)
}
