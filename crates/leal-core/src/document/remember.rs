//! The whole-file part of ADR-0008 decision 8 (task 2.5.3b): a save
//! records the delimiter and header choice in Leal's interpretation
//! attribute when a reopen would guess them differently. A save can tell
//! for first paint at once (`save::AttributePlan`), but a reopen's
//! whole-file review needs the review of the new file, about half a second
//! per 100 MB, which doesn't fit a save's budget. The reading a save makes
//! runs that very review anyway, so when it suggests another delimiter,
//! the attribute is written then, onto the saved file.

use std::io;

use crate::attributes::{Fingerprint, Interpretation};
use crate::source::{INTERPRETATION_ATTRIBUTE_C, OriginalState, look_afresh, same_file};

use super::Document;

impl Document {
    /// Records the delimiter and header choice, with the saved bytes'
    /// fingerprint (ADR-0007), in the interpretation attribute of the file
    /// the last save wrote, when the review of that save's reading
    /// suggests another delimiter: a reopen's review would, too.
    ///
    /// It writes only when:
    /// - the current reading is the one a save made (not read again since);
    /// - its review has finished and suggests another delimiter;
    /// - the save didn't record the delimiter already;
    /// - the file is still the one saved, where it is now (it may have
    ///   been moved): the same inode, length and modification time, and the
    ///   watcher has seen no change.
    ///
    /// A recorded encoding (ADR-0013 decision 2) is kept. Returns whether
    /// it wrote the attribute. It looks at the file, which a network share
    /// can slow down: call it off the main thread.
    ///
    /// # Errors
    ///
    /// If the file can't be looked at, or the system refuses the attribute.
    pub fn remember_reviewed_interpretation(&self) -> io::Result<bool> {
        let reading = self.current();
        if !reading.saved {
            return Ok(false);
        }
        let Some(Ok(review)) = reading.review_job.result() else {
            return Ok(false);
        };
        if review.delimiter_suggestion.is_none() {
            return Ok(false);
        }
        let recorded = reading
            .source
            .attributes()
            .interpretation
            .as_deref()
            .and_then(|value| Interpretation::parse(value).ok());
        if recorded.is_some_and(|recorded| recorded.delimiter.is_some()) {
            return Ok(false);
        }
        let status = self.original.status();
        if status.diverged || status.state != OriginalState::Unchanged {
            return Ok(false);
        }
        let value = Interpretation {
            delimiter: Some(reading.detection.delimiter),
            header: Some(reading.detection.header),
            file: Some(Fingerprint::from_head(&reading.head, reading.source.len())),
            encoding: recorded.and_then(|recorded| recorded.encoding),
        }
        .to_attribute_value();
        let Some(existing) = look_afresh(&status.path)? else {
            return Ok(false);
        };
        if !same_file(&self.original.opened(), existing.identity()) {
            return Ok(false);
        }
        existing.set_attribute(INTERPRETATION_ATTRIBUTE_C, value.as_bytes())?;
        Ok(true)
    }
}
