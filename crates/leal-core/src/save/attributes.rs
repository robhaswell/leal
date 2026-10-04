//! The attributes a save gives the file it writes (ADR-0004 decision 11,
//! ADR-0005 decision 1, ADR-0008 decision 8), so a reopen reads it the way
//! the document did: the same encoding, delimiter and header choice.
//!
//! - **`com.apple.TextEncoding`** records the document's encoding when the
//!   file had the attribute when it was opened (it is updated, ADR-0004
//!   decision 11), when the user chose the encoding, or, without a BOM,
//!   when a reopen would guess another one: from the first 64 KB at first
//!   paint (ADR-0005 decision 4) or from the whole file (ADR-0003 decision
//!   1). A BOM decides the encoding by itself. Otherwise it is removed.
//!   Save As UTF-8 always sets it to UTF-8 (ADR-0008 decision 7).
//! - **The interpretation attribute** records the delimiter and the header
//!   choice, with the new file's fingerprint (ADR-0007 decision 1), when
//!   either was the user's choice or came from the attribute, or when a
//!   reopen's first paint, with the attributes as recorded, would guess
//!   either differently. It records the encoding too, with the
//!   fingerprint, when a reopen would otherwise ignore
//!   `com.apple.TextEncoding` because a byte doesn't decode in it (ADR-0004
//!   decision 11): the tag is then Leal's own and holds (ADR-0013 decision
//!   2). First paint decodes only the first 64 KB and the review the rest,
//!   so a longer file records it whenever its encoding leaves a byte
//!   unassigned. Otherwise the attribute is removed.
//!
//! Removing matters as much as writing: the save copies the old file's
//! extended attributes to the new one, and an old fingerprint or a stale
//! encoding must not survive (ADR-0008 decision 8).
//!
//! **Not here:** ADR-0008 decision 8 also writes the interpretation
//! attribute when a reopen's *whole-file review* would suggest another
//! delimiter. In a file of up to 64 KB first paint sees the whole file, so
//! that is covered. In a larger one it needs the review of the new file,
//! about half a second per 100 MB, which doesn't fit DESIGN §1's 500 ms for
//! a save. The reading the save makes runs that very review anyway: when
//! it suggests another delimiter, the app has the core write the attribute
//! then (`Document::remember_reviewed_interpretation`, task 2.5.3b).

use crate::attributes::{Fingerprint, Interpretation, text_encoding_value};
use crate::detect::{
    Census, Choices, DialectSource, EncodingSource, Hints, detect, tag_must_decode,
};
use crate::dialect::{Bom, Encoding};

/// What a save decides the file's two attributes should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttributePlan {
    /// The encoding `com.apple.TextEncoding` records, or `None` to remove
    /// it.
    pub text_encoding: Option<Encoding>,
    /// What Leal's interpretation attribute records, or `None` to remove
    /// it.
    pub interpretation: Option<Interpretation>,
}

impl AttributePlan {
    /// The `com.apple.TextEncoding` value to write, in TextEdit's form.
    #[must_use]
    pub fn text_encoding_value(&self) -> Option<String> {
        self.text_encoding.map(text_encoding_value)
    }

    /// The interpretation attribute's value to write.
    #[must_use]
    pub fn interpretation_value(&self) -> Option<String> {
        self.interpretation.map(|i| i.to_attribute_value())
    }
}

/// What the plan is decided from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttributeFacts<'a> {
    /// How the document reads the file.
    pub(crate) detection: &'a crate::detect::Detection,
    /// Whether the file had `com.apple.TextEncoding` when it was opened.
    pub(crate) had_text_encoding: bool,
    /// The new file's first 64 KB (or all of it).
    pub(crate) head: &'a [u8],
    /// The new file's length.
    pub(crate) len: u64,
    /// The census of the whole new file, if [`needs_census`] said so.
    pub(crate) census: Option<Census>,
    /// The file was written in UTF-8 whatever the document's encoding (Save
    /// As UTF-8, ADR-0008 decision 7): `com.apple.TextEncoding` is set to
    /// UTF-8, and the BOM is a UTF-8 one if the document had a BOM.
    pub(crate) utf8: bool,
    /// The review of the reading a save made had the delimiter and header
    /// recorded in the attribute (`Document::remember_reviewed_interpretation`):
    /// they are recorded again, as a choice of the attribute's.
    pub(crate) remembered: bool,
}

/// Whether the plan needs the census of the whole new file: only for an
/// encoding that was guessed, in a file without a BOM and without the
/// attribute already, where nothing else decides it.
pub(crate) fn needs_census(detection: &crate::detect::Detection, had_text_encoding: bool) -> bool {
    detection.bom == Bom::None
        && !had_text_encoding
        && detection.encoding_source == EncodingSource::Guess
}

impl AttributePlan {
    /// The plan (see the module docs).
    pub(crate) fn decide(facts: &AttributeFacts<'_>) -> AttributePlan {
        let document = facts.detection;
        let encoding = document.encoding;
        let reopen = |text_encoding: Option<&[u8]>, interpretation: Option<&[u8]>| {
            let hints = Hints {
                text_encoding,
                interpretation,
            };
            detect(facts.head, facts.len, hints, Choices::default())
        };
        let text_encoding = if facts.utf8 {
            // Save As UTF-8 sets it, BOM or not (ADR-0008 decision 7).
            Some(Encoding::Utf8)
        } else if document.bom == Bom::None {
            let first_paint = reopen(None, None).map(|d| d.encoding).ok();
            let whole_file = facts.census.map(Census::guess);
            let record = facts.had_text_encoding
                || document.encoding_source != EncodingSource::Guess
                || first_paint != Some(encoding)
                || whole_file.is_some_and(|guess| guess != encoding);
            record.then_some(encoding)
        } else {
            facts.had_text_encoding.then_some(encoding)
        };
        let value = text_encoding.map(text_encoding_value);
        let tag = value.as_deref().map(str::as_bytes);
        let file = Some(Fingerprint::from_head(facts.head, facts.len));

        // ADR-0013 decision 2: a tag a reopen would ignore is made Leal's
        // own. Past the first 64 KB only the review would find the byte.
        let longer = facts.len > u64::try_from(facts.head.len()).unwrap_or(u64::MAX);
        let own_encoding = text_encoding.filter(|&e| {
            let ignored = reopen(tag, None).is_ok_and(|d| d.encoding != e);
            ignored || (longer && tag_must_decode(e))
        });
        let own = Interpretation {
            file,
            encoding: own_encoding,
            ..Interpretation::default()
        };
        let own_value = own_encoding.map(|_| own.to_attribute_value());

        // The reopen, with the encoding as it will read.
        let reopened = reopen(tag, own_value.as_deref().map(str::as_bytes)).ok();
        let chosen = facts.remembered
            || document.delimiter_source != DialectSource::Guess
            || document.header_source != DialectSource::Guess;
        let differs = reopened
            .is_none_or(|d| d.delimiter != document.delimiter || d.header != document.header);
        let interpretation = if chosen || differs {
            Some(Interpretation {
                delimiter: Some(document.delimiter),
                header: Some(document.header),
                ..own
            })
        } else {
            own_encoding.map(|_| own)
        };
        AttributePlan {
            text_encoding,
            interpretation,
        }
    }
}
